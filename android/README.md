# GridCraft for Android

The Gradle project that packages `apps/gridcraft-android` (the Rust app as a `GameActivity`
shell) into an APK/AAB. CI (`.github/workflows/android.yml`) builds it on every push to the
`android` branch and attaches signed builds to a GitHub Release on `android-v*` tags.

## How it fits together

- `apps/gridcraft-android/src/lib.rs`: `android_main`, the platform services (open, save,
  preferences, URLs) and the JNI bridge to `MainActivity`.
- `app/src/main/java/.../MainActivity.kt`: the Storage Access Framework picker, the "open with"
  and "share to" intents for .xlsx/.csv files, saving into `Downloads/GridCraft/`, and the
  full-screen window.
- `cargo ndk` drops `libgridcraft_android.so` into `app/src/main/jniLibs/arm64-v8a/` (ignored by
  git); Gradle packages it.

## Building locally

Needs the Android SDK (platform 35, build-tools 35), NDK r27, a stable Rust toolchain with the
`aarch64-linux-android` target, and `cargo-ndk`:

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk
export ANDROID_NDK_HOME=$ANDROID_SDK_ROOT/ndk/<version>
cargo ndk -t arm64-v8a --platform 30 -o android/app/src/main/jniLibs build --release -p gridcraft-android
cd android && ./gradlew assembleDebug
```

## Keyboard shortcuts

A Bluetooth or USB keyboard works like on the desktop: the same Excel shortcut table (the
commands in `crates/engine/src/cmd/*`). Cmd in the table means Ctrl on Android.

## Known limits (first version)

- Save, Save As and Save a copy write to `Downloads/GridCraft/<name>` without a dialog (the
  engine writes to a private outbox, the shell moves the file on); saving the same name again
  in one session overwrites it. Export as PDF with an empty path has no destination yet.
- Open Recent lists nothing useful: files opened from the picker arrive as bytes, not paths.
- No TCP control channel (and so no MCP driving the UI) on Android.
- Fonts come from `/system/fonts` (Roboto, Noto Serif, Droid Sans Mono) or egui's built-ins;
  Calibri/Carlito metrics are not available, so column widths can differ a little from Excel.
- The desktop ribbon layout needs a tablet-sized screen; on a phone's cover screen it is cramped.
