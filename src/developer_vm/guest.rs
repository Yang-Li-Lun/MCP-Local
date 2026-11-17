use std::io::Write;

use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use flate2::{Compression, write::GzEncoder};
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    budget::Budget,
    filesystem::{Root, vm_mapping::Plan},
};

use super::{
    Backend, Lifecycle, STOP_UNCONFIRMED,
    session::Mappings,
    spool::{GuestResult, Spool},
};

const BOOTSTRAP: &str = include_str!("guest.ps1");
const WINDOWS_POWERSHELL: &str = r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe";
const CWD: &str = "VM_CWD：工作路徑必須為授權根內既有目錄。";

#[derive(Serialize)]
struct AdapterArguments {
    spool: String,
    cwd: String,
    path: String,
    git_roots: Vec<String>,
}

fn encode_bootstrap(args: AdapterArguments) -> Result<String> {
    let data = STANDARD.encode(serde_json::to_vec(&args)?);
    // The only interpolated value is base64 JSON. In particular, a workspace
    // name can never become PowerShell syntax or a Windows command argument.
    let script = format!(
        "$Mcp = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{data}')) | ConvertFrom-Json\n{BOOTSTRAP}"
    );
    let mut compressed = GzEncoder::new(Vec::new(), Compression::default());
    compressed.write_all(script.as_bytes())?;
    let blob = STANDARD.encode(compressed.finish()?);
    let loader = format!(
        "$source=[IO.MemoryStream]::new([Convert]::FromBase64String('{blob}'));\n\
         $gzip=[IO.Compression.GZipStream]::new($source,[IO.Compression.CompressionMode]::Decompress);\n\
         $reader=[IO.StreamReader]::new($gzip,[Text.Encoding]::UTF8);\n\
         try {{ $script=$reader.ReadToEnd() }} finally {{ $reader.Dispose() }};\n\
         & ([ScriptBlock]::Create($script))"
    );
    Ok(STANDARD.encode(
        loader
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    ))
}

pub(super) struct Guest {
    root_id: String,
    relative_cwd: String,
    plan: Plan,
    spool: Spool,
}

impl Guest {
    pub(super) fn prepare(
        root_id: &str,
        root: &Root,
        working_directory: Option<&str>,
        plan: Plan,
    ) -> Result<Self> {
        let cwd = root
            .open(working_directory.unwrap_or("."), false)
            .context(CWD)?;
        ensure!(cwd.metadata().directory(), "{CWD}");
        let spool = Spool::create(root)?;
        Ok(Self {
            root_id: root_id.into(),
            relative_cwd: cwd.relative,
            plan,
            spool,
        })
    }

    pub(super) fn configuration(&self, budget: &mut Budget) -> Result<String> {
        let spool = self.spool.guest_path(&self.root_id, &self.plan)?;
        let (root, _) = spool.rsplit_once('\\').context(CWD)?;
        let cwd = if self.relative_cwd == "." {
            root.to_owned()
        } else {
            format!(r"{root}\{}", self.relative_cwd.replace('/', "\\"))
        };
        let mut path = Vec::new();
        for toolchain in self.plan.guest_toolchains() {
            path.push(toolchain.clone());
            for suffix in ["cmd", "bin", r"usr\bin", r"mingw64\bin"] {
                path.push(format!(r"{toolchain}\{suffix}"));
            }
        }
        path.extend([
            r"C:\Windows\System32".into(),
            r"C:\Windows".into(),
            r"C:\Windows\System32\WindowsPowerShell\v1.0".into(),
        ]);
        let mut git_roots: Vec<_> = self
            .plan
            .guest_roots()
            .map(|(_, directory)| directory.replace('\\', "/"))
            .collect();
        let git_cwd = cwd.replace('\\', "/");
        if !git_roots.contains(&git_cwd) {
            git_roots.push(git_cwd);
        }
        let command = encode_bootstrap(AdapterArguments {
            spool,
            cwd,
            path: path.join(";"),
            git_roots,
        })?;
        let xml = self.plan.configuration(budget)?;
        let body = xml
            .strip_suffix("</Configuration>")
            .context("VM_CONFIG_INVALID")?;
        Ok(format!(
            "{body}<LogonCommand><Command>{WINDOWS_POWERSHELL} -NoLogo -NoProfile -NonInteractive -EncodedCommand {command}</Command></LogonCommand></Configuration>"
        ))
    }

    pub(super) fn start<B: Backend>(
        &self,
        lifecycle: &mut Lifecycle<B>,
        budget: &mut Budget,
        cancel: &CancellationToken,
    ) -> Result<Uuid> {
        budget.check()?;
        lifecycle.start(&self.configuration(budget)?, cancel)
    }

    pub(super) fn mappings(&self) -> Mappings {
        Mappings {
            guest_roots: self
                .plan
                .guest_roots()
                .map(|(id, path)| (id.into(), path))
                .collect(),
            guest_toolchains: self.plan.guest_toolchains().collect(),
        }
    }

    pub(super) fn close<B: Backend>(&mut self, lifecycle: &mut Lifecycle<B>) -> Result<()> {
        let mut budget = Budget::new(CancellationToken::new());
        self.spool
            .close_after_stop(&mut budget, || lifecycle.stop())
            .context(STOP_UNCONFIRMED)
    }

    pub(super) fn submit(&mut self, command: &str, budget: &mut Budget) -> Result<bool> {
        self.spool
            .request_if_ready(command, &self.root_id, &self.plan, budget)
    }

    pub(super) fn result(&mut self, budget: &mut Budget) -> Result<Option<GuestResult>> {
        self.spool.result(budget)
    }

    pub(super) fn refresh_output(&mut self, budget: &mut Budget) -> Result<()> {
        self.spool.refresh_output(budget)
    }

    pub(super) fn output(&self) -> &[u8] {
        self.spool.output()
    }

    pub(super) fn input(&mut self, text: &[u8], eof: bool, budget: &mut Budget) -> Result<usize> {
        ensure!(
            self.spool.result(budget)?.is_none(),
            "SESSION_FINISHED：VM 已回傳結束狀態。"
        );
        self.spool.write_stdin(text, eof, budget)
    }
}

#[cfg(test)]
mod tests;
