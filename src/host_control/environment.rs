use std::{collections::BTreeMap, ffi::OsString, path::Path};

// Match the legacy host allowlist. Never forward arbitrary SDK keys, security
// channel descriptors, compiler injection variables or PowerShell profiles.
pub const NAMES: &[&str] = &[
    "SYSTEMROOT",
    "WINDIR",
    "SYSTEMDRIVE",
    "COMSPEC",
    "PATH",
    "PATHEXT",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMW6432",
    "PROGRAMDATA",
    "COMMONPROGRAMFILES",
    "COMMONPROGRAMFILES(X86)",
    "COMMONPROGRAMW6432",
    "USERPROFILE",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "TEMP",
    "TMP",
    "PROCESSOR_ARCHITECTURE",
    "PROCESSOR_IDENTIFIER",
    "PROCESSOR_LEVEL",
    "PROCESSOR_REVISION",
    "NUMBER_OF_PROCESSORS",
    "OS",
    "PSMODULEPATH",
];

pub fn build(
    source: impl IntoIterator<Item = (OsString, OsString)>,
    windows: &Path,
    cwd: &Path,
) -> BTreeMap<String, OsString> {
    let mut values = BTreeMap::new();
    for (name, value) in source {
        if let Some(name) = name.to_str() {
            let name = name.to_ascii_uppercase();
            if NAMES.contains(&name.as_str()) {
                values.insert(name, value);
            }
        }
    }
    for (name, value) in [
        ("SYSTEMROOT", windows.as_os_str().to_owned()),
        ("WINDIR", windows.as_os_str().to_owned()),
        (
            "COMSPEC",
            windows.join("System32").join("cmd.exe").into_os_string(),
        ),
        ("MCP_WORKSPACE", cwd.as_os_str().to_owned()),
        ("PYTHONIOENCODING", "utf-8".into()),
        ("PYTHONUTF8", "1".into()),
        ("TEMP", cwd.as_os_str().to_owned()),
        ("TMP", cwd.as_os_str().to_owned()),
    ] {
        values.insert(name.into(), value);
    }
    values
        .entry("PATH".into())
        .or_insert_with(|| windows.join("System32").into_os_string());
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_installed_tools_without_forwarding_secrets_or_injection() {
        let source = [
            ("Path", "C:\\tools;C:\\Windows\\System32"),
            ("USERPROFILE", "C:\\Users\\fixture"),
            ("PSModulePath", "C:\\modules"),
            ("CONTROL_PLANE_API_KEY", "synthetic"),
            ("OPENAI_API_KEY", "synthetic"),
            ("MCP_LOCAL_RUST_SECURITY_CHANNEL", "synthetic"),
            ("RUSTC_WRAPPER", "untrusted"),
            ("NODE_OPTIONS", "untrusted"),
            ("PYTHONPATH", "untrusted"),
            ("COMSPEC", "untrusted"),
        ]
        .map(|(k, v)| (k.into(), v.into()));
        let env = build(source, Path::new("C:/Windows"), Path::new("D:/fixture"));
        assert_eq!(env["PATH"], "C:\\tools;C:\\Windows\\System32");
        assert_eq!(env["USERPROFILE"], "C:\\Users\\fixture");
        assert_eq!(env["PSMODULEPATH"], "C:\\modules");
        assert_eq!(
            env["COMSPEC"],
            Path::new("C:/Windows")
                .join("System32")
                .join("cmd.exe")
                .into_os_string()
        );
        assert_eq!(env["TEMP"], "D:/fixture");
        for key in [
            "CONTROL_PLANE_API_KEY",
            "OPENAI_API_KEY",
            "MCP_LOCAL_RUST_SECURITY_CHANNEL",
            "RUSTC_WRAPPER",
            "NODE_OPTIONS",
            "PYTHONPATH",
        ] {
            assert!(!env.contains_key(key));
        }
    }
}
