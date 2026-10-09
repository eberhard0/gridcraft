//! GridCraft on Android.
//!
//! Runs the same [`gridcraft_ui_egui::SheetApp`] as the desktop app inside a `GameActivity`
//! (android-activity's `game-activity` backend, which eframe needs for the soft keyboard and
//! accesskit). Built with `cargo ndk` into `android/app/src/main/jniLibs`, then packaged by the
//! Gradle project in `android/`.
//!
//! Differences from the desktop app:
//! - no TCP control channel;
//! - File › Open asks `MainActivity.pickOpen()` (Storage Access Framework); the bytes come back
//!   on a Java thread through `nativeDeliverFile` into `Services::inbox`, like the web build;
//! - Save, Save As and Save a copy (CSV/HTML) write to an "outbox" directory in the app's private
//!   files; after each frame the shell moves whatever appeared there to `Downloads/GridCraft/<name>`
//!   through `MainActivity.saveToDownloads` (MediaStore, no dialog). Saving the same name again in
//!   one session overwrites that file;
//! - preferences (`ui.json`, `prefs.json`, as on the desktop) live in the private files directory.

#![cfg(target_os = "android")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use android_activity::AndroidApp;
use gridcraft_engine::Session;
use gridcraft_ui_egui::{Inbox, Services, SheetApp};
use jni::objects::{JByteArray, JObject, JString};
use jni::{Env, EnvUnowned, JavaVM, jni_sig, jni_str};

const LOG_TAG: &str = "gridcraft";
/// How often the preferences are written while the app runs (Android may kill the process
/// without calling `on_exit`).
const PREFS_INTERVAL: Duration = Duration::from_secs(30);

/// Files the Kotlin side delivers (name, bytes); the app opens them on its next frame.
static INBOX: OnceLock<Inbox> = OnceLock::new();
/// The egui context, to wake the app when a file arrives from a Java thread.
static CTX: OnceLock<egui::Context> = OnceLock::new();
/// The process's Java VM (set once) and the current activity (a reference android-activity
/// owns, stored as an address; 0 = none).
static VM: OnceLock<JavaVM> = OnceLock::new();
static ACTIVITY: Mutex<usize> = Mutex::new(0);

fn inbox() -> &'static Inbox {
    INBOX.get_or_init(Inbox::default)
}

/// The activity's entry point, called by android-activity's GameActivity glue on its own thread.
/// It returns when the activity is destroyed.
#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    static LOGGER: OnceLock<()> = OnceLock::new();
    LOGGER.get_or_init(|| {
        android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag(LOG_TAG));
    });
    // SAFETY: `vm_as_ptr` is the process's JavaVM, valid for the life of the process.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let _ = VM.set(vm);
    *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner) = app.activity_as_ptr() as usize;

    let data_dir = app.internal_data_path().unwrap_or_else(|| PathBuf::from("/data/local/tmp"));
    log::info!("GridCraft {} starting; data in {}", env!("CARGO_PKG_VERSION"), data_dir.display());
    let options = eframe::NativeOptions {
        android_app: Some(app),
        // eframe saves egui panel/window sizes here on exit.
        persistence_path: Some(data_dir.join("ui.ron")),
        ..Default::default()
    };
    let result = eframe::run_native(
        "GridCraft",
        options,
        Box::new(move |cc| {
            let _ = CTX.set(cc.egui_ctx.clone());
            let outbox = data_dir.join("outbox");
            let mut session = Session::new();
            session.new_workbook();
            let mut app = SheetApp::new(session, services(&outbox));
            load_prefs(&mut app, &data_dir);
            SheetApp::setup_context(&cc.egui_ctx, app.ui.dark);
            Ok(Box::new(Shell { app, outbox, data_dir, prefs_written: Instant::now() }))
        }),
    );
    *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner) = 0;
    if let Err(e) = result {
        log::error!("GridCraft stopped: {e}");
        // winit allows one event loop per process: when Android recreates the activity in the
        // same process, end the process so the next launch starts clean instead of a blank window.
        std::process::exit(0);
    }
}

/// The eframe app: the spreadsheet plus the outbox → Downloads hand-off and the preferences.
struct Shell {
    app: SheetApp,
    outbox: PathBuf,
    data_dir: PathBuf,
    prefs_written: Instant,
}

impl eframe::App for Shell {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.app.logic(ctx);
        flush_outbox(&self.outbox);
        if self.prefs_written.elapsed() >= PREFS_INTERVAL {
            save_prefs(&self.app, &self.data_dir);
            self.prefs_written = Instant::now();
        }
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.app.raw_input_hook(raw);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
    }

    fn on_exit(&mut self) {
        flush_outbox(&self.outbox);
        save_prefs(&self.app, &self.data_dir);
    }
}

/// The platform services: the picker and URLs go through the activity; saves land in `outbox`.
fn services(outbox: &Path) -> Services {
    let outbox = outbox.to_path_buf();
    Services {
        // The picker is asynchronous: the file arrives later through the inbox.
        open_async: Some(Box::new(|| {
            if let Err(e) = pick_open() {
                log::error!("couldn't open the file picker: {e}");
            }
        })),
        // No save dialog: the suggested name becomes the file name in Downloads/GridCraft (the
        // engine writes it to the outbox first; `flush_outbox` moves it on).
        pick_save: Some(Box::new(move |suggested: &str| {
            if let Err(e) = std::fs::create_dir_all(&outbox) {
                log::error!("couldn't create {}: {e}", outbox.display());
                return None;
            }
            Some(outbox.join(file_name(suggested)).to_string_lossy().to_string())
        })),
        open_url: Some(Box::new(|url: &str| {
            if let Err(e) = open_url(url) {
                log::error!("couldn't open {url}: {e}");
            }
        })),
        inbox: Some(inbox().clone()),
        ..Default::default()
    }
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).filter(|n| !n.is_empty()).unwrap_or_else(|| "Book1.xlsx".to_string())
}

/// Moves every file the engine wrote to the outbox into `Downloads/GridCraft/`.
fn flush_outbox(outbox: &Path) {
    let Ok(entries) = std::fs::read_dir(outbox) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        match std::fs::read(&path) {
            Ok(bytes) => match save_to_downloads(&name, &bytes) {
                Ok(()) => log::info!("saved {name} ({} bytes) to Downloads", bytes.len()),
                Err(e) => log::error!("couldn't save {name} to Downloads: {e}"),
            },
            Err(e) => log::error!("couldn't read {}: {e}", path.display()),
        }
        // The copy in the outbox is only a staging file; the document keeps its path so Save
        // (Ctrl+S) writes it again and overwrites the Downloads file.
        if let Err(e) = std::fs::remove_file(&path) {
            log::warn!("couldn't remove {}: {e}", path.display());
        }
    }
}

// ---- Preferences (same files as the desktop app) ----------------------------------------------

fn load_prefs(app: &mut SheetApp, dir: &Path) {
    if let Ok(b) = std::fs::read(dir.join("ui.json"))
        && let Ok(ui) = serde_json::from_slice(&b)
    {
        app.ui = ui;
    }
    if let Ok(b) = std::fs::read(dir.join("prefs.json"))
        && let Ok(p) = serde_json::from_slice(&b)
    {
        app.session.prefs = p;
    }
}

fn save_prefs(app: &SheetApp, dir: &Path) {
    if let Ok(b) = serde_json::to_vec_pretty(&app.ui)
        && let Err(e) = write_atomic(&dir.join("ui.json"), &b)
    {
        log::warn!("couldn't save ui.json: {e}");
    }
    if let Ok(b) = serde_json::to_vec_pretty(&app.session.prefs)
        && let Err(e) = write_atomic(&dir.join("prefs.json"), &b)
    {
        log::warn!("couldn't save prefs.json: {e}");
    }
}

/// Write `bytes` to `path` through a temporary file, so a crash mid-write keeps the old file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

// ---- Calls into MainActivity (Kotlin) ----------------------------------------------------------

/// Run `f` with a JNI environment on this thread and the current activity.
fn with_activity<T>(f: impl FnOnce(&mut Env<'_>, &JObject<'_>) -> jni::errors::Result<T>) -> Result<T, String> {
    let vm = VM.get().ok_or("the Java VM is not available")?;
    let raw = *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner);
    if raw == 0 {
        return Err("the activity is not running".to_string());
    }
    let raw = raw as jni::sys::jobject;
    vm.attach_current_thread(|env| -> jni::errors::Result<T> {
        // SAFETY: the reference comes from android-activity's `activity_as_ptr`, which keeps it
        // valid while the activity runs (ACTIVITY is cleared when `run_native` returns). `Cast`
        // neither owns nor deletes it.
        let activity = unsafe { env.as_cast_raw::<JObject>(&raw)? };
        f(env, &activity)
    })
    .map_err(|e| e.to_string())
}

/// `MainActivity.pickOpen()`: show the system file picker; the result comes through the inbox.
fn pick_open() -> Result<(), String> {
    with_activity(|env, activity| {
        env.call_method(activity, jni_str!("pickOpen"), jni_sig!("()V"), &[])?;
        Ok(())
    })
}

/// `MainActivity.saveToDownloads(name, bytes)`: `null` on success, else the error message.
fn save_to_downloads(name: &str, bytes: &[u8]) -> Result<(), String> {
    with_activity(|env, activity| {
        let jname = JString::from_str(env, name)?;
        let jbytes = env.byte_array_from_slice(bytes)?;
        let ret = env
            .call_method(activity, jni_str!("saveToDownloads"), jni_sig!("(Ljava/lang/String;[B)Ljava/lang/String;"), &[(&jname).into(), (&jbytes).into()])?
            .l()?;
        if ret.is_null() {
            return Ok(Ok(()));
        }
        let message = env.cast_local::<JString>(ret)?;
        Ok(Err(message.to_string()))
    })?
}

/// `MainActivity.openUrl(url)`: Help and About links in the browser.
fn open_url(url: &str) -> Result<(), String> {
    with_activity(|env, activity| {
        let jurl = JString::from_str(env, url)?;
        env.call_method(activity, jni_str!("openUrl"), jni_sig!("(Ljava/lang/String;)V"), &[(&jurl).into()])?;
        Ok(())
    })
}

// ---- Calls from MainActivity (Kotlin) ----------------------------------------------------------

/// `MainActivity.nativeDeliverFile(name, bytes)`: a picked, viewed or shared file's contents,
/// from a Java thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_iameberhard_gridcraft_MainActivity_nativeDeliverFile<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _this: JObject<'caller>,
    name: JString<'caller>,
    bytes: JByteArray<'caller>,
) {
    let outcome = unowned_env.with_env(|env| -> jni::errors::Result<()> {
        let name = name.to_string();
        let bytes = env.convert_byte_array(&bytes)?;
        log::info!("received {name} ({} bytes)", bytes.len());
        inbox().lock().unwrap_or_else(PoisonError::into_inner).push((name, bytes));
        if let Some(ctx) = CTX.get() {
            ctx.request_repaint();
        }
        Ok(())
    });
    outcome.resolve::<jni::errors::LogErrorAndDefault>()
}
