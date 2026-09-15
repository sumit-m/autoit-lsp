//! Static list of AutoIt v3 built-in macros for completion.
//!
//! Source: https://www.autoitscript.com/autoit3/docs/macros.htm
//!
//! Each entry is `(name_with_at, short_description)`.  Names are stored
//! with their `@` prefix and official spelling so they can be emitted as-is
//! in completion items. Lookup is by lowercase name (AutoIt macros are
//! case-insensitive).

/// One macro entry: `(name, description)`.
pub struct MacroDoc {
    pub name: &'static str,
    pub description: &'static str,
}

/// All built-in AutoIt macros, in the order of the official reference page.
pub static MACROS: &[MacroDoc] = &[
    MacroDoc { name: "@AppDataCommonDir",   description: "Path to the common Application Data folder" },
    MacroDoc { name: "@AppDataDir",         description: "Path to the current user's Roaming Application Data folder" },
    MacroDoc { name: "@AutoItExe",          description: "Full path of the running AutoIt executable or compiled script" },
    MacroDoc { name: "@AutoItPID",          description: "Process ID of the current script" },
    MacroDoc { name: "@AutoItVersion",      description: "Version number of AutoIt (e.g. 3.3.16.1)" },
    MacroDoc { name: "@AutoItX64",          description: "Returns 1 if the script runs under native x64 AutoIt" },
    MacroDoc { name: "@COM_EventObj",       description: "Object the COM event is fired on (only valid in a COM event function)" },
    MacroDoc { name: "@CommonFilesDir",     description: "Path to the Common Files folder" },
    MacroDoc { name: "@Compiled",           description: "Returns 1 if compiled (.exe or .a3x), 0 for an .au3 file" },
    MacroDoc { name: "@ComputerName",       description: "Computer's network name" },
    MacroDoc { name: "@ComSpec",            description: "Value of %COMSPEC%, the command interpreter (e.g. cmd.exe)" },
    MacroDoc { name: "@CPUArch",            description: "CPU architecture: \"X86\" or \"X64\"" },
    MacroDoc { name: "@CR",                 description: "Carriage return character (\\r, ASCII 13)" },
    MacroDoc { name: "@CRLF",               description: "Carriage return + line feed (\\r\\n)" },
    MacroDoc { name: "@DesktopCommonDir",   description: "Path to the common Desktop folder" },
    MacroDoc { name: "@DesktopDepth",       description: "Depth of the primary display in bits per pixel" },
    MacroDoc { name: "@DesktopDir",         description: "Path to the current user's Desktop folder" },
    MacroDoc { name: "@DesktopHeight",      description: "Height of the primary display in pixels" },
    MacroDoc { name: "@DesktopRefresh",     description: "Refresh rate of the primary display in hertz" },
    MacroDoc { name: "@DesktopWidth",       description: "Width of the primary display in pixels" },
    MacroDoc { name: "@DocumentsCommonDir", description: "Path to the common Documents folder" },
    MacroDoc { name: "@error",              description: "Status of the error flag, set by functions on failure" },
    MacroDoc { name: "@exitCode",           description: "Exit code as set by the Exit statement" },
    MacroDoc { name: "@exitMethod",         description: "How the script exited (see OnAutoItExitRegister)" },
    MacroDoc { name: "@extended",           description: "Extended return value set by some functions (e.g. StringReplace)" },
    MacroDoc { name: "@FavoritesCommonDir", description: "Path to the common Favorites folder" },
    MacroDoc { name: "@FavoritesDir",       description: "Path to the current user's Favorites folder" },
    MacroDoc { name: "@GUI_CtrlHandle",     description: "Handle of the last clicked GUI control (OnEvent functions only)" },
    MacroDoc { name: "@GUI_CtrlId",         description: "ID of the last clicked GUI control (OnEvent functions only)" },
    MacroDoc { name: "@GUI_DragFile",       description: "Filename of the file being dropped (drop event only)" },
    MacroDoc { name: "@GUI_DragId",         description: "ID of the GUI control being dragged (drop event only)" },
    MacroDoc { name: "@GUI_DropId",         description: "ID of the GUI control dropped onto (drop event only)" },
    MacroDoc { name: "@GUI_WinHandle",      description: "Handle of the last clicked GUI window (OnEvent functions only)" },
    MacroDoc { name: "@HomeDrive",          description: "Drive letter of the current user's home directory" },
    MacroDoc { name: "@HomePath",           description: "Directory part of the current user's home directory" },
    MacroDoc { name: "@HomeShare",          description: "Server and share name of the current user's home directory" },
    MacroDoc { name: "@HotKeyPressed",      description: "Last hotkey pressed (see HotKeySet)" },
    MacroDoc { name: "@HOUR",               description: "Current hour (00–23)" },
    MacroDoc { name: "@IPAddress1",         description: "IP address of the first network adapter" },
    MacroDoc { name: "@IPAddress2",         description: "IP address of the second network adapter" },
    MacroDoc { name: "@IPAddress3",         description: "IP address of the third network adapter" },
    MacroDoc { name: "@IPAddress4",         description: "IP address of the fourth network adapter" },
    MacroDoc { name: "@KBLayout",           description: "Keyboard layout code" },
    MacroDoc { name: "@LF",                 description: "Line feed character (\\n, ASCII 10)" },
    MacroDoc { name: "@LocalAppDataDir",    description: "Path to the current user's Local Application Data folder" },
    MacroDoc { name: "@LogonDNSDomain",     description: "Logon DNS domain" },
    MacroDoc { name: "@LogonDomain",        description: "Logon domain" },
    MacroDoc { name: "@LogonServer",        description: "Logon server" },
    MacroDoc { name: "@MDAY",               description: "Current day of the month (01–31)" },
    MacroDoc { name: "@MIN",                description: "Current minute (00–59)" },
    MacroDoc { name: "@MON",                description: "Current month (01–12)" },
    MacroDoc { name: "@MSEC",               description: "Current millisecond (000–999)" },
    MacroDoc { name: "@MUILang",            description: "Multilingual User Interface (MUI) language code" },
    MacroDoc { name: "@MyDocumentsDir",     description: "Path to the current user's My Documents folder" },
    MacroDoc { name: "@NumParams",          description: "Number of parameters passed to the current user function" },
    MacroDoc { name: "@OSArch",             description: "OS architecture: \"X86\", \"IA64\" or \"X64\"" },
    MacroDoc { name: "@OSBuild",            description: "OS build number" },
    MacroDoc { name: "@OSLang",             description: "OS language code" },
    MacroDoc { name: "@OSServicePack",      description: "Service pack info (e.g. \"Service Pack 3\")" },
    MacroDoc { name: "@OSType",             description: "Returns \"WIN32_NT\" on NT-based Windows versions" },
    MacroDoc { name: "@OSVersion",          description: "Windows version (e.g. \"WIN_11\", \"WIN_10\", \"WIN_2022\")" },
    MacroDoc { name: "@ProgramFilesDir",    description: "Path to the Program Files folder" },
    MacroDoc { name: "@ProgramsCommonDir",  description: "Path to the common Start Menu Programs folder" },
    MacroDoc { name: "@ProgramsDir",        description: "Path to the current user's Start Menu Programs folder" },
    MacroDoc { name: "@ScriptDir",          description: "Directory of the running script (no trailing backslash)" },
    MacroDoc { name: "@ScriptFullPath",     description: "Full path of the running script" },
    MacroDoc { name: "@ScriptLineNumber",   description: "Line number being executed (uncompiled scripts only)" },
    MacroDoc { name: "@ScriptName",         description: "Filename of the running script" },
    MacroDoc { name: "@SEC",                description: "Current second (00–59)" },
    MacroDoc { name: "@StartMenuCommonDir", description: "Path to the common Start Menu folder" },
    MacroDoc { name: "@StartMenuDir",       description: "Path to the current user's Start Menu folder" },
    MacroDoc { name: "@StartupCommonDir",   description: "Path to the common Startup folder" },
    MacroDoc { name: "@StartupDir",         description: "Path to the current user's Startup folder" },
    MacroDoc { name: "@SW_DISABLE",         description: "Disable the window" },
    MacroDoc { name: "@SW_ENABLE",          description: "Enable the window" },
    MacroDoc { name: "@SW_HIDE",            description: "Hide the window and activate another window" },
    MacroDoc { name: "@SW_LOCK",            description: "Lock the window to prevent repainting" },
    MacroDoc { name: "@SW_MAXIMIZE",        description: "Activate and maximize the window" },
    MacroDoc { name: "@SW_MINIMIZE",        description: "Minimize the window and activate the next top-level window" },
    MacroDoc { name: "@SW_RESTORE",         description: "Activate and restore the window from minimized or maximized" },
    MacroDoc { name: "@SW_SHOW",            description: "Activate and show the window at its current size and position" },
    MacroDoc { name: "@SW_SHOWDEFAULT",     description: "Use the show state set by the program that started the application" },
    MacroDoc { name: "@SW_SHOWMAXIMIZED",   description: "Activate and show the window maximized" },
    MacroDoc { name: "@SW_SHOWMINIMIZED",   description: "Activate and show the window minimized" },
    MacroDoc { name: "@SW_SHOWMINNOACTIVE", description: "Show the window minimized without activating it" },
    MacroDoc { name: "@SW_SHOWNA",          description: "Show the window in its current state without activating it" },
    MacroDoc { name: "@SW_SHOWNOACTIVATE",  description: "Show the window at its most recent size/position without activating it" },
    MacroDoc { name: "@SW_SHOWNORMAL",      description: "Activate and show the window, restoring it if minimized or maximized" },
    MacroDoc { name: "@SW_UNLOCK",          description: "Unlock the window to allow repainting" },
    MacroDoc { name: "@SystemDir",          description: "Path to the Windows System32 folder" },
    MacroDoc { name: "@TAB",                description: "Tab character (ASCII 9)" },
    MacroDoc { name: "@TempDir",            description: "Path to the temporary files folder" },
    MacroDoc { name: "@TRAY_ID",            description: "ID of the last clicked tray item" },
    MacroDoc { name: "@TrayIconFlashing",   description: "Returns 1 if the tray icon is flashing" },
    MacroDoc { name: "@TrayIconVisible",    description: "Returns 1 if the tray icon is visible" },
    MacroDoc { name: "@UserName",           description: "Login name of the current user" },
    MacroDoc { name: "@UserProfileDir",     description: "Path to the current user's profile folder" },
    MacroDoc { name: "@WDAY",               description: "Day of the week (1=Sunday … 7=Saturday)" },
    MacroDoc { name: "@WindowsDir",         description: "Path to the Windows folder" },
    MacroDoc { name: "@WorkingDir",         description: "Current working directory of the script" },
    MacroDoc { name: "@YDAY",               description: "Day of the year (001–366)" },
    MacroDoc { name: "@YEAR",               description: "Current four-digit year" },
];

/// Case-insensitive lookup of a macro by name (with or without `@` prefix).
///
/// The macro-hover counterpart of [`crate::builtins::lookup`]; retained (and
/// tested) for when hover surfaces macro docs — not yet wired into a handler.
#[allow(dead_code)]
pub fn lookup(name: &str) -> Option<&'static MacroDoc> {
    let normalised = if name.starts_with('@') {
        name.to_lowercase()
    } else {
        format!("@{}", name.to_lowercase())
    };
    MACROS.iter().find(|m| m.name.to_lowercase() == normalised)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crlf_is_present() {
        assert!(lookup("@CRLF").is_some());
    }

    #[test]
    fn lookup_without_at_prefix() {
        assert!(lookup("CRLF").is_some());
    }

    #[test]
    fn lookup_is_case_insensitive() {
        assert!(lookup("@crlf").is_some());
        assert!(lookup("@ScriptDir").is_some());
        assert!(lookup("@scriptdir").is_some());
    }

    #[test]
    fn unknown_macro_returns_none() {
        assert!(lookup("@NotAMacro").is_none());
    }

    #[test]
    fn catalog_matches_official_macro_reference() {
        assert_eq!(MACROS.len(), 102);
        for fake in ["@NUL", "@NetworkDrive", "@NetworkShare"] {
            assert!(lookup(fake).is_none(), "{fake} is not an AutoIt macro");
        }
        for real in [
            "@GUI_CtrlHandle", "@GUI_CtrlId", "@GUI_DragFile", "@GUI_DragId",
            "@GUI_DropId", "@GUI_WinHandle", "@SW_LOCK", "@SW_UNLOCK",
        ] {
            assert!(lookup(real).is_some(), "{real} missing");
        }
        assert_eq!(lookup("@OSTYPE").map(|m| m.name), Some("@OSType"));
    }

    #[test]
    fn macro_names_are_unique() {
        let mut names: Vec<String> = MACROS.iter().map(|m| m.name.to_lowercase()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), MACROS.len());
    }
}
