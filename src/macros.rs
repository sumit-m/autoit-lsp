//! Static list of AutoIt v3 built-in macros for completion.
//!
//! Source: https://www.autoitscript.com/autoit3/docs/macros.htm
//!
//! Each entry is `(name_with_at, short_description)`.  Names are stored
//! with their `@` prefix so they can be emitted as-is in completion items.
//! Lookup is by lowercase name (AutoIt macros are case-insensitive).

/// One macro entry: `(name, description)`.
pub struct MacroDoc {
    pub name: &'static str,
    pub description: &'static str,
}

/// All built-in AutoIt macros. Ordered alphabetically by name.
pub static MACROS: &[MacroDoc] = &[
    MacroDoc { name: "@AppDataCommonDir",  description: "Path to Application Data (common)" },
    MacroDoc { name: "@AppDataDir",        description: "Path to current user Application Data folder" },
    MacroDoc { name: "@AutoItExe",         description: "Full path of the AutoIt executable or compiled script" },
    MacroDoc { name: "@AutoItPID",         description: "Process ID of the current AutoIt process" },
    MacroDoc { name: "@AutoItVersion",     description: "Version number of AutoIt (e.g. 3.3.16.1)" },
    MacroDoc { name: "@AutoItX64",         description: "Returns 1 if the script is running under 64-bit AutoIt" },
    MacroDoc { name: "@COM_EventObj",      description: "Object the COM event came from (only valid in a COM event function)" },
    MacroDoc { name: "@CommonFilesDir",    description: "Path to Common Files folder" },
    MacroDoc { name: "@Compiled",          description: "Returns 1 if the script is a compiled executable" },
    MacroDoc { name: "@ComputerName",      description: "NetBIOS name of the computer" },
    MacroDoc { name: "@ComSpec",           description: "Value of %COMSPEC%, e.g. C:\\Windows\\System32\\cmd.exe" },
    MacroDoc { name: "@CPUArch",           description: "CPU architecture: \"X86\" or \"X64\"" },
    MacroDoc { name: "@CR",               description: "Carriage return character (\\r, ASCII 13)" },
    MacroDoc { name: "@CRLF",             description: "Carriage return + line feed (\\r\\n)" },
    MacroDoc { name: "@DesktopCommonDir",  description: "Path to the common Desktop folder" },
    MacroDoc { name: "@DesktopDepth",      description: "Depth of the primary display in bits per pixel" },
    MacroDoc { name: "@DesktopDir",        description: "Path to the current user Desktop folder" },
    MacroDoc { name: "@DesktopHeight",     description: "Height of the primary display in pixels" },
    MacroDoc { name: "@DesktopRefresh",    description: "Refresh rate of the primary display in Hz" },
    MacroDoc { name: "@DesktopWidth",      description: "Width of the primary display in pixels" },
    MacroDoc { name: "@DocumentsCommonDir", description: "Path to the common Documents folder" },
    MacroDoc { name: "@error",            description: "Status of the error flag — set by functions on failure" },
    MacroDoc { name: "@exitCode",         description: "Exit code for the current script — set by Exit statement" },
    MacroDoc { name: "@exitMethod",       description: "How the script was closed (0=natural, 1=close, 2=logoff, etc.)" },
    MacroDoc { name: "@extended",         description: "Extended return value from the last function call" },
    MacroDoc { name: "@FavoritesCommonDir",  description: "Path to the common Favorites folder" },
    MacroDoc { name: "@FavoritesDir",      description: "Path to the current user Favorites folder" },
    MacroDoc { name: "@HomeDrive",         description: "Drive letter of the home directory, e.g. C:" },
    MacroDoc { name: "@HomePath",          description: "Directory part of the home directory path" },
    MacroDoc { name: "@HomeShare",         description: "Server and share name of the home directory" },
    MacroDoc { name: "@HotKeyPressed",     description: "Last hotkey string that triggered the current function" },
    MacroDoc { name: "@HOUR",             description: "Current hour (00–23)" },
    MacroDoc { name: "@IPAddress1",        description: "IP address of the first network adapter" },
    MacroDoc { name: "@IPAddress2",        description: "IP address of the second network adapter" },
    MacroDoc { name: "@IPAddress3",        description: "IP address of the third network adapter" },
    MacroDoc { name: "@IPAddress4",        description: "IP address of the fourth network adapter" },
    MacroDoc { name: "@KBLayout",          description: "Keyboard layout ID for the current user" },
    MacroDoc { name: "@LF",               description: "Line feed character (\\n, ASCII 10)" },
    MacroDoc { name: "@LocalAppDataDir",   description: "Path to Local Application Data folder" },
    MacroDoc { name: "@LogonDNSDomain",    description: "DNS name of the domain to which the user is logged on" },
    MacroDoc { name: "@LogonDomain",       description: "Domain name used for login" },
    MacroDoc { name: "@LogonServer",       description: "Name of the server that authenticated the current login" },
    MacroDoc { name: "@MDAY",             description: "Current day of the month (01–31)" },
    MacroDoc { name: "@MIN",              description: "Current minute (00–59)" },
    MacroDoc { name: "@MON",              description: "Current month (01–12)" },
    MacroDoc { name: "@MSEC",             description: "Current millisecond (000–999)" },
    MacroDoc { name: "@MUILang",           description: "MUI language code, e.g. 0409 for English" },
    MacroDoc { name: "@MyDocumentsDir",    description: "Path to the current user My Documents folder" },
    MacroDoc { name: "@NetworkDrive",      description: "Drive letter of the network home directory" },
    MacroDoc { name: "@NetworkShare",      description: "UNC path of the network home directory" },
    MacroDoc { name: "@NUL",              description: "Path to the NUL device (discards output)" },
    MacroDoc { name: "@NumParams",         description: "Number of parameters passed to the current user function" },
    MacroDoc { name: "@OSArch",            description: "OS architecture: \"X86\" or \"X64\"" },
    MacroDoc { name: "@OSBuild",           description: "Windows build number" },
    MacroDoc { name: "@OSLang",            description: "Default OS language code" },
    MacroDoc { name: "@OSServicePack",     description: "Service pack string, e.g. \"Service Pack 1\"" },
    MacroDoc { name: "@OSTYPE",            description: "\"WIN32_NT\" for all modern Windows versions" },
    MacroDoc { name: "@OSVersion",         description: "Windows version string, e.g. \"WIN_10\"" },
    MacroDoc { name: "@ProgramFilesDir",   description: "Path to Program Files folder" },
    MacroDoc { name: "@ProgramsCommonDir", description: "Path to common Programs (Start menu) folder" },
    MacroDoc { name: "@ProgramsDir",       description: "Path to current user Programs (Start menu) folder" },
    MacroDoc { name: "@ScriptDir",         description: "Directory of the currently executing script (no trailing backslash)" },
    MacroDoc { name: "@ScriptFullPath",    description: "Full path of the currently executing script" },
    MacroDoc { name: "@ScriptLineNumber",  description: "Current line number in the script" },
    MacroDoc { name: "@ScriptName",        description: "Filename of the currently executing script" },
    MacroDoc { name: "@SEC",              description: "Current second (00–59)" },
    MacroDoc { name: "@StartMenuCommonDir",  description: "Path to the common Start Menu folder" },
    MacroDoc { name: "@StartMenuDir",      description: "Path to the current user Start Menu folder" },
    MacroDoc { name: "@StartupCommonDir",  description: "Path to the common Startup folder" },
    MacroDoc { name: "@StartupDir",        description: "Path to the current user Startup folder" },
    MacroDoc { name: "@SW_DISABLE",        description: "Constant for disabling a window (6)" },
    MacroDoc { name: "@SW_ENABLE",         description: "Constant for enabling a window (9)" },
    MacroDoc { name: "@SW_HIDE",           description: "Constant for hiding a window (0)" },
    MacroDoc { name: "@SW_MAXIMIZE",       description: "Constant for maximizing a window (3)" },
    MacroDoc { name: "@SW_MINIMIZE",       description: "Constant for minimizing a window (6)" },
    MacroDoc { name: "@SW_RESTORE",        description: "Constant for restoring a window (9)" },
    MacroDoc { name: "@SW_SHOW",           description: "Constant for showing a window (5)" },
    MacroDoc { name: "@SW_SHOWDEFAULT",    description: "Constant for showing a window using its default state (10)" },
    MacroDoc { name: "@SW_SHOWMAXIMIZED",  description: "Constant for showing a window maximized (3)" },
    MacroDoc { name: "@SW_SHOWMINIMIZED",  description: "Constant for showing a window minimized (2)" },
    MacroDoc { name: "@SW_SHOWMINNOACTIVE",  description: "Show window minimized without activating it (7)" },
    MacroDoc { name: "@SW_SHOWNA",         description: "Show window in its current state without activating (8)" },
    MacroDoc { name: "@SW_SHOWNOACTIVATE", description: "Show window in most recent size/position without activating (4)" },
    MacroDoc { name: "@SW_SHOWNORMAL",     description: "Constant for showing a window normally (1)" },
    MacroDoc { name: "@SystemDir",         description: "Path to the Windows System32 directory" },
    MacroDoc { name: "@TAB",              description: "Tab character (ASCII 9)" },
    MacroDoc { name: "@TempDir",           description: "Path to the Windows temporary directory" },
    MacroDoc { name: "@TRAY_ID",          description: "Last tray item that was clicked (ID)" },
    MacroDoc { name: "@TrayIconFlashing",  description: "Returns 1 if the tray icon is flashing" },
    MacroDoc { name: "@TrayIconVisible",   description: "Returns 1 if the tray icon is visible" },
    MacroDoc { name: "@UserName",          description: "Login name of the current user" },
    MacroDoc { name: "@UserProfileDir",    description: "Path to the current user profile directory" },
    MacroDoc { name: "@WDAY",             description: "Day of the week (1=Sunday … 7=Saturday)" },
    MacroDoc { name: "@WindowsDir",        description: "Path to the Windows directory" },
    MacroDoc { name: "@WorkingDir",        description: "Current working directory of the script" },
    MacroDoc { name: "@YDAY",             description: "Day of the year (001–366)" },
    MacroDoc { name: "@YEAR",             description: "Current four-digit year" },
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
    fn macro_list_is_nonempty() {
        assert!(MACROS.len() > 50, "expected at least 50 macros");
    }
}
