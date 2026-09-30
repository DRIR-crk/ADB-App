# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [2.5.0] - 2026-09-30

### Added
- Phones that open **Pair device with pairing code** on the same network are now detected automatically (mDNS, like Android Studio): they show up in the new *Pairing code* tab and as a notice anywhere in the app, so only the 6-digit code has to be typed. Android never broadcasts that code, so it cannot be detected. Can be turned off in Settings (*Detect pairing requests*).
- After pairing, the device is connected automatically (adb's own auto-connect is given a moment, otherwise the phone's connection port is used).
- Progress events for pairing, and localized messages for wrong code / unreachable device / not paired / expired QR.

### Changed
- New wireless dialog with three tabs: *Pairing code*, *QR* and *Manual*.
- QR pairing was rewritten: crisp vector QR with a proper quiet zone, one cancellable session that waits up to 5 minutes (it used to regenerate every 30 seconds, and could loop forever on errors), and discovery that no longer depends on the exact text format of `adb mdns services`.
- Much faster startup: the main JavaScript bundle went from 2.1 MB to 0.3 MB (device names, the debloat list and the QR generator are loaded on demand).
- Device change events from `adb track-devices` are coalesced, and the tracker process is stopped when the app exits (it used to stay running and keep `adb.exe` locked).
- Every adb command is killed if its caller is cancelled and never waits for input.
- Updating or installing ADB / scrcpy always uses the app's managed folder (never an SDK or system directory), keeps a rollback copy, retries locked files on Windows and works across drives.
- Much smaller permission set: `fs:default` was replaced by the two file permissions actually used.

### Fixed
- Installing APKs: failed installs were shown as successful; `xhdpi`/`xxhdpi`/`xxxhdpi` split APKs were never installed; only the preferred ABI of a bundle is installed now.
- Sideload: the progress bar never moved, the button was only offered in a state where adb refuses sideload, and progress/cancel were lost when leaving the Home tab.
- Files: the address bar listed the folder on every keystroke; Delete/Enter typed in the search box could delete the selected file; names with spaces, quotes, parentheses or backslashes; previews/thumbnails of such files; very large folders (sorting on every keystroke); unbounded thumbnail memory and requests; keyboard navigation below the first screen; downloads on macOS/Linux; old Android `ls` formats.
- Apps: endless refresh loop when the list came back empty; uninstall / clear data reported success when they failed; apps without icon; double click also toggled the selection; stale metadata overwriting newer flags; filter counters.
- Settings: saving any setting reverted a custom ADB/scrcpy path; moving the data folder could wipe it (it is now validated first and aborted on any error); the per-app language switch did nothing; Update was offered for system-installed tools.
- Device selector: any key press on the disconnect button disconnected the device, and clicking it also selected that device.
- Media volume always showed 7/15 below Android 16; text typed on the device was not shell-quoted; system page failed entirely when a single query was unsupported; brightness slider with adaptive brightness; slider jumping while dragging.
- Transfer queue: removing a running transfer did not cancel it and retrying a running one started it twice.
- App cleanup (temporary files, the adb tracker and, if enabled, the adb server) now also runs when the app exits without a window-close event, e.g. Cmd+Q on macOS.

### Security
- The webview can no longer ask the backend to delete an arbitrary folder (`close_app`), and file names coming from the device are sanitized and executable types are never opened directly.
- Package names and permission names are validated before being used in device shell commands; the icon protocol only serves plain package names.

## [2.4.1] - 2026-07-19

### Changed
- The saved screenshots now save with the date and time like when saving manually.
- Improved wireless device connection.

## [2.4.0] - 2026-07-11

### Added
- The operations menu can now be closed using the ESC key.
- It is now possible to update custom path components.
- Uploaded files now appear immediately in the file explorer without needing to refresh (similar to Windows Explorer).
- ADB App now remembers its window state before closing.

### Changed
- Temporarily reverted to a standard assets.car icon on macOS due to a Tauri bug preventing the Liquid Glass icon from generating correctly.
- Combined app and file drag-and-drop mechanics for better visual coherence.
- The advanced ADB input is now closed by default and resizes automatically.
- "Quick display settings" are now grouped under "Dimension" for clarity.

### Fixed
- Fixed issues when detecting dependencies on macOS.
- Fixed path detection issues on macOS.
- Fixed some issues related to searching for updates on ADB (due to Google's XML format).
- Fixed Linux build and resolved Linux dependencies.
- ADB App now renders correctly on Windows 10 Fall Creators Update and older.
- Fixed the Microsoft Store build.
- Fixed a visual bug in the top-left corner.
- Fixed TargetSDK and MinSDK detection.
- The Android TV remote no longer displays a loading screen when clicking a button.

### Removed
- Removed support for installing `.aab` packages due to low usage, the Java dependency requirement, and lack of online distribution.

## [2.3.0] - 2026-07-02

### Added
- Animation speed and font size can be personalized from Display.
- Display now has more visual coherence.
- Data updates automatically on the main page (improved) and Control.

### Changed
- Updated the app icon to a liquid glass design for macOS.
- The marketing name of the device is now displayed in the topbar.
- App icons are now saved in WebP format, significantly improving loading speeds and reducing space.
- Operations are now performed in Rust for better performance and stability.
- The app now automatically switches to a newly connected device if the previous USB device is disconnected.
- Optimized overall app loading times.
- Icons cache is now enabled by default.

### Fixed
- Fixed issues preventing proper application installations.
- Cleaned up redundant success states in the system.
- The Liquid Glass icon for macOS is shown again.

## [2.2.0] - 2026-06-29

### Added
- Added automatic saving of screenshots directly to the pictures folder.
- Updated the app icon to a liquid glass design for macOS.

### Changed
- App icons are now saved in WebP format, significantly improving loading speeds and reducing space.
- Operations are now performed in Rust for better performance and stability.
- The app now automatically switches to a newly connected device if the previous USB device is disconnected.
- The marketing name of the device is now displayed in the topbar.
- Optimized overall app loading times.
- Icons cache is now enabled by default.

### Fixed
- Fixed issues preventing proper application installations.
- Cleaned up redundant success states in the system.

## [2.1.0] - 2026-06-26

### Added
- Added advanced options containing useful commands, the first being a captive portal activator (useful for WearOS).
- The marketing name of the device is now shown on home.
- Added uninstall and reinstall menus for system apps.
- Added a debloat option in apps.
- Added search on APKMirror or Web from Apps.
- Double clicking a file opens it if it's less than 25 mb.
- Microsoft Store version includes an option to rate the app in the Microsoft Store.
- Microsoft Store version includes Scrcpy and ADB are included by default.

### Changed
- New app icon.
- Transfers are now background operations and also include app installations; they are situated in the topbar for easy access from anywhere.
- Improved wireless connection stability.
- App cache now only saves the app name to reuse the cache across different devices.
- In mirroring, the app name appears if it is loaded.
- Drastically improved the loading speed of apps and files when there are many elements.
- The QR code for wireless pairing is now generated directly by JS.
- The wireless debugging dialog box it's simpler.
- Folder transfers in files now try to resume where they left off.
- The Home screen now only executes a single command for better performance.
- Replaced the manual browser shortcut detector with `tauri-plugin-prevent-default`.
- Improved how scrcpy shows the currently installed version and optimized getting all dependencies.

### Fixed
- Fixed several bugs with the contextual menu of files.
- Fixed the installation of APKs and handled related errors.
- Fixed Svelte errors and replaced heavy regex for better performance.

### Removed
- `tar` and `flate2` are no longer included unless the platform is macOS.

## [2.0.0] - 2026-06-20

### Added
- The program detects and adds devices automatically upon connection, no manual refresh needed.
- The system recognizes and displays the name of installed keyboards through their package.
- Added the option to choose between responsive or normal mode in "View on PC" within the apps section.
- Implemented a dedicated file transfer menu.
- Incorporated direct preview for audio files within the file explorer.
- Better screenshot UI and a dedicated button to download the current wallpaper.
- Visual customization: implemented the Acrylic effect for Windows 10 and as a selector in Windows 11, along with a button to disable it on macOS and Windows.
- Added terms of use licenses.

### Changed
- Apps can be mirrored in normal or desktop mode. Double-clicking will select desktop mode, with normal mode as the fallback.
- Improved file navigation using keyboard arrows.
- Complete Material You redesign: the interface now adapts its colors based on the connected device's wallpaper, and the remaining native dialogs were replaced by Material dialogs.
- Redesigned right screen on home: shows current wallpaper, the device status, and has options for Recovery mode such as adb sideload.
- The updater was implemented.
- The main page reloads data automatically from time to time.
- Improved the design of the APK installation menu.
- Migration to Svelte: frontend complete redesign achieving a massive performance boost, much faster app load times...
- Internationalization optimization: migrated to `paraglide-js` for better performance. Less reliance on declaring languages in files, and ADB error messages are now translated as well.
- Much faster loading of devices, wallpapers, and the app list.
- CSS combined directly into the Svelte files.

### Fixed
- Fixed the macOS traffic lights.
- Due to the migration, many bugs appeared and disappeared, so they are not counted here.

### Removed
- The `window-vibrancy` dependency was removed and replaced with native Tauri.

## [1.0.0] - 2026-06-11
- Initial release.
