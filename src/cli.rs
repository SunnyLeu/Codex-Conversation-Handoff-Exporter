//! Explicit mode dispatch. Clap validates arity, types and conflicts first.
//! Engineering flags never silently grant write permission in product mode.
use crate::Args;
use clap::{CommandFactory, FromArgMatches, Parser, parser::ValueSource};
use std::ffi::OsString;

const PUBLIC: &[&str] = &[
    "project_id",
    "thread_id",
    "output_directory",
    "yes",
    "developer_mode",
    "help",
    "version",
];
const REQUIRES_DEVELOPER: &[&str] = &[
    "no_write",
    "single_session",
    "sessions_root",
    "session_index",
    "project_state",
    "attachment_root",
    "legacy_cwd_only",
    "target_cwd",
    "benchmark_runs",
    "hotspot_profile",
    "compare_reference",
    "list_projects",
];
#[derive(Debug)]
pub(crate) struct ProductOptions {
    pub(crate) project: Option<String>,
    pub(crate) thread: Option<String>,
    pub(crate) output: Option<std::path::PathBuf>,
    pub(crate) yes: bool,
}
#[derive(Debug)]
pub(crate) enum Invocation {
    Product(ProductOptions),
    Developer(Args),
}
fn bad(message: impl Into<String>) -> clap::Error {
    clap::Error::raw(clap::error::ErrorKind::ArgumentConflict, message.into())
}

pub(crate) fn parse(raw: Vec<OsString>) -> Result<Invocation, clap::Error> {
    let dev = raw.iter().any(|s| s == "--developer-mode");
    let mut command = Args::command();
    if !dev {
        let private: Vec<String> = command
            .get_arguments()
            .filter(|arg| !PUBLIC.contains(&arg.get_id().as_str()))
            .map(|a| a.get_id().as_str().to_owned())
            .collect();
        for id in private {
            command = command.mut_arg(&id, |a| a.hide(true));
        }
        command=command.about("Codex 對話匯出器：側邊欄目標、原生資料夾選擇、人工確認")
            .after_help("不指定目標：數字選單；不指定目的地：3 秒後開啟資料夾選擇器。\n預設必須按 E 才開始；--yes 必須同時提供來源與目的地。\n工程說明：--developer-mode --help");
    }
    let matches = command.try_get_matches_from(raw)?;
    let args = Args::from_arg_matches(&matches)?;
    let given = |key: &str| matches.value_source(key) == Some(ValueSource::CommandLine);
    if let Some(id) = args.project_id.as_deref().or(args.thread_id.as_deref()) {
        if id.trim().is_empty() || id.chars().any(char::is_control) {
            return Err(bad("來源 ID 不可空白或含控制字元"));
        }
    }
    if given("output_directory") && args.output_directory.as_os_str().is_empty() {
        return Err(bad("--output-directory 不可空白"));
    }
    if given("target_cwd") && args.target_cwd.trim().is_empty() {
        return Err(bad("--target-cwd 不可空白"));
    }
    if args.developer_mode {
        let writes = !args.list_projects
            && !args.no_write
            && args.benchmark_runs == 0
            && !args.hotspot_profile;
        if writes && (!(given("project_id") || given("target_cwd")) || !given("output_directory")) {
            return Err(bad(
                "開發者模式的寫入必須明確指定 --project-id 或 --target-cwd，以及 --output-directory；不使用隱含來源或目的地。",
            ));
        }
        return Ok(Invocation::Developer(args));
    }
    for key in REQUIRES_DEVELOPER {
        if given(key) {
            return Err(bad(format!(
                "--{} 必須搭配 --developer-mode；未執行任何匯出。",
                key.replace('_', "-")
            )));
        }
    }
    let result = ProductOptions {
        project: args.project_id,
        thread: args.thread_id,
        output: given("output_directory").then_some(args.output_directory),
        yes: args.yes,
    };
    if result.yes
        && (result.project.is_none() && result.thread.is_none() || result.output.is_none())
    {
        return Err(bad(
            "--yes 必須同時指定一個來源（--project-id 或 --thread-id）及 --output-directory；不會啟動互動選單。",
        ));
    }
    Ok(Invocation::Product(result))
}

pub(crate) fn entry(raw: Vec<OsString>) -> i32 {
    let dev = raw.iter().any(|s| s == "--developer-mode");
    let unattended = raw.iter().any(|s| s == "--yes");
    let no_pause = raw.iter().any(|s| s == "--no-pause");
    match parse(raw) {
        Ok(Invocation::Developer(args)) => {
            crate::legacy_main(args);
            0
        }
        Ok(Invocation::Product(options)) => crate::product::start(options),
        Err(error) => {
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) {
                let _ = error.print();
                return 0;
            }
            std::eprintln!("[參數錯誤] {}", error.to_string().trim());
            use std::io::IsTerminal;
            if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
                if dev && !no_pause {
                    std::eprintln!("Press Enter to close...");
                    let mut line = String::new();
                    let _ = std::io::stdin().read_line(&mut line);
                } else if !dev && !unattended {
                    crate::console_ui::wait_q();
                }
            }
            2
        }
    }
}

pub(crate) fn product_args(options: &ProductOptions) -> anyhow::Result<Args> {
    crate::codex_paths::home()?;
    let mut args = Args::try_parse_from(["Codex-Conversation-Handoff-Exporter"])?;
    args.project_id = options.project.clone();
    args.thread_id = options.thread.clone();
    args.output_directory = options.output.clone().unwrap_or_default();
    args.no_pause = true;
    args.product_mode = true;
    // Normal product use is not the old oversubscribed benchmark profile.
    args.workers = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4)
        .min(32);
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn p(values: &[&str]) -> Result<Invocation, clap::Error> {
        parse(
            std::iter::once("test")
                .chain(values.iter().copied())
                .map(OsString::from)
                .collect(),
        )
    }
    #[test]
    fn all_eight_source_destination_yes_combinations() {
        for src in [false, true] {
            for dst in [false, true] {
                for yes in [false, true] {
                    let mut a = vec![];
                    if src {
                        a.extend(["--project-id", "p"]);
                    }
                    if dst {
                        a.extend(["--output-directory", "out"]);
                    }
                    if yes {
                        a.push("--yes");
                    }
                    assert_eq!(p(&a).is_ok(), !yes || (src && dst), "{a:?}");
                }
            }
        }
    }
    #[test]
    fn product_ignores_tuning_not_control_flow() {
        let Invocation::Product(x) = p(&["--workers=8", "--verbose", "--profile-phases"]).unwrap()
        else {
            panic!()
        };
        assert!(x.project.is_none());
        for flag in [
            "--no-write",
            "--list-projects",
            "--legacy-cwd-only",
            "--hotspot-profile",
        ] {
            assert!(p(&[flag]).is_err());
        }
        for flag in [
            "--sessions-root",
            "--project-state",
            "--single-session",
            "--compare-reference",
        ] {
            assert!(p(&[flag, "x"]).is_err());
        }
    }
    #[test]
    fn unknown_duplicate_conflicting_and_missing_values_rejected() {
        for flags in [
            &["--unknown"][..],
            &["--workers"],
            &["--workers=bad"],
            &["--project-id", "a", "--project-id", "b"],
            &["--project-id", "a", "--thread-id", "b"],
            &["--output-directory", ""],
        ] {
            assert!(p(flags).is_err(), "{flags:?}");
        }
    }
    #[test]
    fn developer_write_gate_and_mode_conflicts() {
        assert!(p(&["--developer-mode"]).is_err());
        assert!(p(&["--developer-mode", "--list-projects"]).is_ok());
        assert!(p(&["--developer-mode", "--no-write"]).is_ok());
        assert!(
            p(&[
                "--developer-mode",
                "--project-id=p",
                "--output-directory=out"
            ])
            .is_ok()
        );
        assert!(p(&["--developer-mode", "--yes"]).is_err());
        assert!(p(&["--developer-mode", "--thread-id=t"]).is_err());
    }
    #[test]
    fn info_has_no_execution_side_effects() {
        assert_eq!(
            p(&["--help"]).unwrap_err().kind(),
            clap::error::ErrorKind::DisplayHelp
        );
        assert_eq!(
            p(&["--version"]).unwrap_err().kind(),
            clap::error::ErrorKind::DisplayVersion
        );
    }
}
