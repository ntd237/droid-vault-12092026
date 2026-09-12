# PROMPT: Build "Droid Vault" — ADB App Uninstaller / Restorer (Windows Desktop)

## 1. Objective

Build a Windows desktop application named **Droid Vault** that connects to an Android phone via USB (ADB), lists all installed apps with their **exact icon, app label, and package name** as shown on the device, splits them into **User apps** and **System apps** tabs, and lets the user multi-select apps to **uninstall for user 0** or **restore** them — individually or in batch.

The app must be **lightweight** (small .exe, low RAM) and have a **modern, beautiful UI**.

## 2. Locked Technology Stack

- **Framework**: Tauri 2 (Rust backend + web frontend rendered in Windows WebView2).
- **Frontend**: React + TypeScript + TailwindCSS. UI text in **Vietnamese**.
- **Backend (Rust)**:
  - ADB interaction via `std::process::Command` calling `adb.exe` (from PATH; also allow a configured/bundled platform-tools path).
  - APK metadata parsing via the `apk-info-axml` crate (AXML + ARSC parsing for app label and icon). Fallback: `axmldecoder` if needed.
  - Icons sent to the frontend as base64 data URLs.
- **Cache**: icon + label cached on disk under the OS AppData dir, keyed by `package_name + versionCode`, so APKs are pulled only once per version.
- Target: build to a single `.exe` via `tauri build` (expected size 3–10 MB, low RAM usage).

## 3. Domain Knowledge & Exact ADB Commands

ADB provides **no direct command for app names or icons** — only package names. The correct data pipeline is:

1. List packages:
   - User apps: `adb shell pm list packages -3`
   - System apps: `adb shell pm list packages -s`
   - Include packages uninstalled for the user: `adb shell pm list packages -u --user 0`
2. Per package, get APK path: `adb shell pm path <package>` — modern apps are **split APKs** (e.g. `split_config.arm64_v8a.apk`); **only pull `base.apk`** (enough for label + icon).
3. Pull `base.apk` to a temp dir, then parse label + icon from the APK itself — this guarantees the icon and name match exactly what the device launcher shows (same APK resources). Note: labels are the APK's default-locale label, which is what the launcher shows; some apps legitimately have non-English default labels (e.g. `钱包`).
4. Detect restorable apps: a package uninstalled via `pm uninstall -k --user 0` still exists on the system (data kept). It disappears from the default package list but appears with `-u`. Verify per package via `adb shell dumpsys package <package>` → line `User 0: ... installed=false`.
5. Operations:
   - Uninstall for user 0 (keeps data, reversible): `adb shell pm uninstall -k --user 0 <package>`
   - Restore: `adb shell cmd package install-existing <package>`

## 4. Functional Requirements

### 4.1 Device connection
- Poll `adb devices` every 2 seconds.
- Show connection status with 3 states: **Connected** (show device model), **Unauthorized** (instruction: "Mở điện thoại và bấm Cho phép để xác nhận gỡ lỗi USB"), **No device** (instruction to enable USB debugging and plug the cable).
- Handle cable disconnection at any moment without crashing; reset UI state gracefully.

### 4.2 App list UI (modeled after the "Package Names" Android app)
- Two tabs: **USER APPS** and **SYSTEM APPS**.
- Each row/card: app **icon** (real icon from the APK), **app label**, **package name** (secondary line, monospace), and a **checkbox**.
- Badge per app: normal, or **"Đã gỡ (khôi phục được)"** for packages uninstalled for user 0 but restorable.
- Instant client-side search filtering across label and package name.
- Select-all / clear-selection per tab; selection survives tab switching.

### 4.3 Uninstall / Restore
- Action bar with two batch buttons: **Gỡ cài đặt** (uninstall) and **Khôi phục** (restore), enabled when selection is non-empty; each button shows the selected count.
- Confirmation dialog before executing, listing chosen packages; **extra warning when any system app is selected**.
- Execute commands **sequentially** (never issue parallel ADB commands to the same device). Show a progress bar (n/total) and a per-package result log line (success / failure + raw ADB error).
- After completion, refresh app states.

### 4.4 Safety
- **Blacklist**: never allow uninstalling core packages, at minimum: `android`, `com.android.systemui`, `com.android.settings`, `com.android.phone`, `com.android.providers.telephony`, `com.android.providers.contacts`, and the launcher package. Show a blocked message if attempted.
- Restorable-but-installed state: `cmd package install-existing` on a non-uninstalled package fails — handle and surface the error message instead of crashing.

### 4.5 Performance
- Cache parsed icons/labels in AppData keyed by `package + versionCode`; on re-open, only new/changed apps require an APK pull.
- Only pull `base.apk` (skip split APKs). Optional optimization if easy: check for `/system/bin/aapt` on device and use `adb shell aapt dump badging <apk path>` to read label/icon without pulling, with the pull-based path as fallback.
- Run the initial scan in the background with a progress indicator; UI must stay responsive while APKs are pulled.

## 5. UI / UX Design Requirements

- Modern web-quality design: dark/light theme (dark default), rounded cards, subtle shadows, smooth transitions, sticky header with connection status chip, sticky bottom action bar.
- Layout: left sidebar or top header (connection status + device info + refresh), main area = tab bar + search box + app list, bottom action bar (selection count + Uninstall/Restore buttons).
- Long package lists (200+ apps) must scroll smoothly (virtualized list if needed).
- All UI text in Vietnamese.

## 6. Implementation Phases (build and verify in this order)

1. **Project skeleton + device detection** — Tauri 2 + React scaffold; Rust command wrapping `adb devices`; UI shows the 3 connection states, polling every 2s. *Verify: plug/unplug phone, status updates; unauthorized state shows the instruction.*
2. **App listing** — list user/system packages, pull `base.apk`, parse label + icon, render tabbed list with icons, names, package names, checkboxes, search. *Verify: counts match `pm list packages -3` / `-s`; icons and labels visually match the phone launcher.*
3. **Restorable-state detection** — `-u` listing + `dumpsys` check; show "Đã gỡ (khôi phục được)" badges. *Verify: manually uninstall one app via ADB, reopen app, badge appears.*
4. **Batch uninstall/restore** — confirmation dialog, sequential execution, progress bar, per-package result log, state refresh. *Verify: uninstall 2–3 apps in one batch, then restore them in one batch; both succeed and UI updates.*
5. **Polish + safety** — blacklist enforcement, disconnect mid-operation handling, persistent icon cache, empty/error states, `tauri build` producing the final `.exe`, README with build + usage instructions (including the ColorOS/Oppo note: must also enable "USB debugging (Security settings)" for `pm` commands to work).

## 7. Constraints

- Do not use Electron or any heavy runtime; the final binary must stay small (Tauri 2 as locked above).
- Do not parallelize ADB commands against one device.
- Do not run any uninstall/restore without explicit user confirmation.
- Do not invent ADB flags beyond the documented ones in section 3 without verifying them against the device.
- Code must be cleanly separated: Rust ADB service layer, Rust APK parser module, Rust cache module; frontend talks to backend only through Tauri commands/events.

## 8. Acceptance Criteria

- [ ] With a phone connected and USB debugging on, the app shows the device as connected within ~2 seconds.
- [ ] User and System tabs list every package returned by `pm list packages -3` / `-s`, each with correct real icon, correct app label, and package name.
- [ ] An app uninstalled with `pm uninstall -k --user 0` is shown with a restorable badge and can be restored with `cmd package install-existing` from the UI.
- [ ] Multi-select uninstall of 2+ apps works in one operation with progress and per-app results.
- [ ] Blacklisted core packages cannot be uninstalled.
- [ ] Unplugging the phone mid-operation does not crash the app.
- [ ] `tauri build` outputs a working `.exe`; second launch reuses the icon cache (no re-pull for unchanged apps).
