# Token Pet

Token Pet is a compact Tauri desktop widget for cc-switch usage stats. It stays
on top, hides to the tray, remembers its position, and shows today's token usage
with a small mascot UI.

## Run

```powershell
npm install
npm run dev
```

## Build

Use a clean Cargo home if your global Cargo registry points to an unavailable
mirror:

```powershell
npm run build
```

The portable executable is produced at:

```text
src-tauri\target\release\Token Pet.exe
```

The release exe uses the system WebView2 runtime and is much smaller than the
Electron build. On current Windows 10/11 systems, WebView2 is usually already
installed.

## Data Source

Token Pet reads the current user's cc-switch database:

```text
%USERPROFILE%\.cc-switch\cc-switch.db
```

You can override it with:

```powershell
$env:CC_SWITCH_DB = "D:\path\to\cc-switch.db"
```
