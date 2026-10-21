//! The user-facing state machine. No JSONL conversion code lives here.
use crate::{
    cli::ProductOptions,
    console_ui::{self, Screen},
    sidebar_targets::{self, Catalogue, Kind, Target},
};
use anyhow::{Result, bail};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct Confirmation {
    output: Option<PathBuf>,
    auto_pending: bool,
}
impl Confirmation {
    fn new(output: Option<PathBuf>) -> Self {
        Self {
            auto_pending: output.is_none(),
            output,
        }
    }
    fn picker_result(&mut self, selected: Option<PathBuf>) {
        self.auto_pending = false;
        if let Some(p) = selected {
            self.output = Some(p);
        }
    }
    fn open_picker(&mut self) -> bool {
        let was = self.auto_pending;
        self.auto_pending = false;
        was
    }
}
enum ConfirmResult {
    Export(PathBuf),
    Back,
    Quit,
}

pub(crate) fn start(options: ProductOptions) -> i32 {
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !options.yes && !interactive {
        std::eprintln!(
            "[錯誤] 非互動環境不能等待人工確認；請明確指定來源、--output-directory 及 --yes。"
        );
        return 2;
    }
    let visual = interactive && !options.yes;
    let mode = crate::platform::ConsoleMode::new(visual);
    let screen = Screen {
        ansi: mode.ansi,
        visual,
    };
    console_ui::capture();
    let result = run(&screen, &options, visual);
    match result {
        Ok(()) => 0,
        Err(error) => {
            screen.header("匯出未完成");
            screen.status("失敗", &error.to_string(), 91);
            screen.line(&format!(
                "本次已完成最終寫入：{} 份。未宣稱整批回滾；請保留既有輸出供核對。",
                crate::progress::written()
            ));
            if let Some(path) = console_ui::save_error_log(&format!("{error:#}")) {
                screen.line(&format!("診斷紀錄：{}", path.display()));
            }
            if options.yes {
                std::println!(
                    "V2_RESULT {}",
                    serde_json::json!({"status":"FAIL","version":crate::VERSION,"error":format!("{error:#}"),"completed_writes":crate::progress::written()})
                );
            }
            if visual {
                crate::platform::clear_input();
                console_ui::wait_q();
            }
            1
        }
    }
}
fn read_catalogue(args: &crate::Args, _screen: &Screen, _visual: bool) -> Result<Catalogue> {
    // Only lightweight metadata is read here. Do not start the export progress
    // worker (which adds a 120 ms join delay) before the menu is even visible.
    let began = Instant::now();
    let result = sidebar_targets::load(args);
    if std::env::var_os("CODEX_HANDOFF_SIDEBAR_TIMING").is_some() {
        std::println!(
            "SIDEBAR_LOAD_MS={:.3}",
            began.elapsed().as_secs_f64() * 1000.0
        );
    }
    result
}
fn run(screen: &Screen, options: &ProductOptions, visual: bool) -> Result<()> {
    let mut args = crate::cli::product_args(options)?;
    crate::configure_runtime(&args);
    crate::configure_attachment_root(&args.attachment_root)?;
    let mut catalogue = read_catalogue(&args, screen, visual)?;
    let mut selected = match (&options.project, &options.thread) {
        (Some(id), None) => Some(catalogue.select(Kind::Project, id)?),
        (None, Some(id)) => Some(catalogue.select(Kind::IndependentThread, id)?),
        (None, None) => None,
        _ => bail!("一次只能選擇一個匯出目標"),
    };
    if options.yes {
        let target = selected.ok_or_else(|| anyhow::anyhow!("--yes 缺少來源"))?;
        let output = options
            .output
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--yes 缺少目的地"))?;
        execute(
            screen,
            &mut args,
            &target,
            &output,
            &catalogue.fingerprint,
            visual,
            true,
        )?;
        return Ok(());
    }
    loop {
        let target = if let Some(target) = selected.take() {
            target
        } else {
            let Some(t) = menu(screen, &catalogue)? else {
                return Ok(());
            };
            t
        };
        match confirmation(screen, &target, options.output.clone(), &args)? {
            ConfirmResult::Quit => return Ok(()),
            ConfirmResult::Back => {
                catalogue = read_catalogue(&args, screen, visual)?;
                continue;
            }
            ConfirmResult::Export(output) => {
                // Resolve again after human input. Never retain a stale index or
                // accept a source that disappeared/moved while the picker was open.
                let fresh = read_catalogue(&args, screen, visual)?;
                let current = fresh.select(target.kind.clone(), &target.id)?;
                execute(
                    screen,
                    &mut args,
                    &current,
                    &output,
                    &fresh.fingerprint,
                    visual,
                    false,
                )?;
                crate::platform::clear_input();
                console_ui::wait_q();
                return Ok(());
            }
        }
    }
}
fn menu(screen: &Screen, catalogue: &Catalogue) -> Result<Option<Target>> {
    screen.header("對話匯出器");
    screen.line("請選擇本次匯出目標（數字後按 Enter）");
    let mut group = "";
    for (i, t) in catalogue.targets.iter().enumerate() {
        let next = match t.kind {
            Kind::IndependentThread => "獨立對話",
            Kind::Project if t.pinned => "已釘選專案",
            Kind::Project => "其他專案",
        };
        if next != group {
            screen.line("");
            screen.section(next);
            group = next;
        }
        let kind = if t.kind == Kind::Project {
            "專案"
        } else {
            "對話"
        };
        screen.choice(i + 1, kind, &t.title);
        // Paths and short IDs distinguish otherwise identical labels.
        screen.tint(&format!("      {}", t.detail), 90);
    }
    if catalogue.targets.is_empty() {
        screen.status("提醒", "目前沒有可確認的匯出目標", 33);
    }
    if !catalogue.unavailable.is_empty() {
        screen.line("");
        screen.section("目前無法匯出的側邊欄項目：");
        for (_, title, reason) in &catalogue.unavailable {
            screen.line(&format!(
                "  {title}（{}）",
                crate::console_ui::clipped(reason, 100)
            ));
        }
        for (id, _, reason) in &catalogue.unavailable {
            crate::console_ui::legacy_line(true, format_args!("Unavailable {id}: {reason}"));
        }
    }
    screen.line("");
    screen.action("Q", "離開（按 Enter 確認）", 97);
    loop {
        screen.prompt("請輸入編號或 Q，按 Enter 確認：")?;
        let Some(input) = console_ui::command(None)? else {
            continue;
        };
        if input.eq_ignore_ascii_case("q") {
            return Ok(None);
        }
        if let Ok(n) = input.parse::<usize>() {
            if n > 0 {
                if let Some(t) = catalogue.targets.get(n - 1) {
                    return Ok(Some(t.clone()));
                }
            }
        }
        screen.status("提醒", "請輸入清單中的數字，或按 Q 離開。", 33);
    }
}
fn confirmation(
    screen: &Screen,
    target: &Target,
    output: Option<PathBuf>,
    args: &crate::Args,
) -> Result<ConfirmResult> {
    let mut state = Confirmation::new(output);
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut notice: Option<(String, u8)> = None;
    loop {
        screen.header("匯出確認");
        screen.line(&format!(
            "匯出類型  {}",
            if target.kind == Kind::Project {
                "整個專案"
            } else {
                "獨立對話"
            }
        ));
        screen.tint(&format!("目標名稱  {}", target.title), 97);
        match &state.output {
            Some(p) => {
                screen.section("匯出位置");
                screen.tint(&p.to_string_lossy(), 96);
            }
            None => screen.tint("匯出位置  尚未選擇", 93),
        }
        if state.auto_pending {
            screen.tint(
                "3 秒後開啟資料夾選擇視窗；可輸入 F、B 或 Q 並按 Enter。",
                93,
            );
        }
        if let Some((message, color)) = notice.take() {
            screen.status("提示", &message, color);
        }
        screen.line("");
        screen.action("F", "選擇資料夾", 96);
        screen.action(
            "E",
            "開始匯出",
            if state.output.is_some() { 92 } else { 90 },
        );
        screen.action("B", "返回選單", 97);
        screen.action("Q", "離開", 97);
        screen.prompt("請輸入指令，按 Enter 確認：")?;
        let input = console_ui::command(
            state
                .auto_pending
                .then(|| deadline.saturating_duration_since(Instant::now())),
        )?;
        match input.as_deref() {
            None | Some("F") => {
                state.open_picker();
                std::println!();
                screen.line("請在 Windows 視窗選擇資料夾；取消會返回本頁。");
                match crate::platform::pick_folder() {
                    Ok(p) => {
                        notice = Some((
                            if p.is_some() {
                                "資料夾已選擇；仍須輸入 E 並按 Enter 開始。"
                            } else {
                                "已取消選擇資料夾，未進行匯出。"
                            }
                            .to_owned(),
                            if p.is_some() { 92 } else { 93 },
                        ));
                        state.picker_result(p);
                    }
                    Err(e) => notice = Some((format!("{e:#}"), 93)),
                }
            }
            Some("B") => return Ok(ConfirmResult::Back),
            Some("Q") => return Ok(ConfirmResult::Quit),
            Some("E") => {
                let Some(p) = &state.output else {
                    notice = Some(("尚未選擇匯出資料夾；輸入 F 並按 Enter 選擇。".into(), 93));
                    continue;
                };
                let checked = crate::destination::absolute(p).and_then(|p| {
                    crate::destination::validate_path(&p, args)?;
                    // At E (never before), check existing Handoff ownership so
                    // a cross-target folder remains a recoverable F/B choice.
                    // A new/empty folder skips the expensive metadata scan;
                    // build_all_candidates still enforces it for every export.
                    if p.is_dir() {
                        let mut existing_handoff = false;
                        for entry in std::fs::read_dir(&p)? {
                            let name = entry?.file_name();
                            if name
                                .to_string_lossy()
                                .to_ascii_lowercase()
                                .ends_with(".handoff.json")
                            {
                                existing_handoff = true;
                                break;
                            }
                        }
                        if existing_handoff {
                            let state = crate::project_membership::read_stable_state(
                                &crate::project_state_path(args),
                            )?;
                            let files = crate::discover_rollouts(&args.sessions_root)?;
                            let scope = if target.kind == Kind::Project {
                                crate::project_membership::resolve_state(
                                    &files,
                                    &state,
                                    "",
                                    Some(&target.id),
                                )?
                            } else {
                                crate::project_membership::resolve_independent(
                                    &files, &state, &target.id,
                                )?
                            };
                            if scope.root_ids.is_empty() {
                                bail!("此專案目前沒有可匯出的主對話");
                            }
                            crate::destination::validate_existing(&p, &scope.selected_ids)?;
                        }
                    }
                    Ok(p)
                });
                match checked {
                    Ok(p) => return Ok(ConfirmResult::Export(p)),
                    Err(e) => notice = Some((format!("無法使用此目錄：{e:#}"), 93)),
                }
            }
            _ => notice = Some(("請使用 F、E、B 或 Q，並按 Enter 確認。".into(), 93)),
        }
    }
}
fn execute(
    screen: &Screen,
    args: &mut crate::Args,
    target: &Target,
    output: &std::path::Path,
    fingerprint: &str,
    visual: bool,
    yes: bool,
) -> Result<()> {
    args.output_directory = crate::destination::absolute(output)?;
    args.project_id = (target.kind == Kind::Project).then(|| target.id.clone());
    args.thread_id = (target.kind == Kind::IndependentThread).then(|| target.id.clone());
    args.expected_state = Some(fingerprint.into());
    crate::destination::validate_path(&args.output_directory, args)?;
    // build_all_candidates resolves the complete logical scope and calls
    // validate_existing *before* creating, writing or cleaning Handoffs.
    screen.header("正在匯出");
    screen.tint(&format!("目標  {}", target.title), 97);
    screen.line("正在處理資料；執行期間不接受 Q 中斷。");
    let reporter = crate::progress::Reporter::start(screen.ansi, visual);
    let began = Instant::now();
    let result = crate::build_all_candidates(args, true);
    reporter.finish();
    let (candidates, metrics) = result?;
    if metrics.scan_errors > 0
        || metrics.export_errors > 0
        || metrics.outputs_rewritten != metrics.root_conversations
    {
        bail!(
            "匯出未完整完成：{}／{}；部分輸出可能已寫入。",
            metrics.outputs_rewritten,
            metrics.root_conversations
        );
    }
    drop(candidates);
    let elapsed = began.elapsed().as_secs_f64();
    screen.header("匯出結果");
    screen.status("成功", "對話匯出完成", 92);
    screen.tint(
        &format!(
            "Handoff     {} / {}",
            metrics.outputs_rewritten, metrics.root_conversations
        ),
        92,
    );
    screen.line(&format!("整合子代理  {}", metrics.integrated_subagents));
    screen.line(&format!("整合續接    {}", metrics.continuations_merged));
    screen.line("掃描／匯出錯誤  0 / 0");
    screen.line(&format!("處理時間    {elapsed:.3} 秒"));
    screen.line("儲存位置");
    screen.tint(&args.output_directory.to_string_lossy(), 96);
    // The Windows synthetic UI qualification provides a fresh, unique path per
    // case. Export metadata only: no message content, user paths beyond the
    // selected output location, or behavior change when the variable is unset.
    // create_new prevents accidentally overwriting an existing diagnostics file.
    if let Some(path) = std::env::var_os("CODEX_HANDOFF_NATIVE_UI_EVIDENCE") {
        let evidence = serde_json::json!({
            "status": "PASS", "version": crate::VERSION,
            "kind": if target.kind == Kind::Project { "project" } else { "independent-thread" },
            "target_id": target.id,
            "output_directory": args.output_directory,
            "handoffs_written": metrics.outputs_rewritten
        });
        if let (Ok(mut file), Ok(bytes)) = (
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path),
            serde_json::to_vec_pretty(&evidence),
        ) {
            let _ = file.write_all(&bytes);
            let _ = file.write_all(b"\n");
        }
    }
    if yes {
        std::println!(
            "V2_RESULT {}",
            serde_json::json!({
                "status":"PASS","version":crate::VERSION,"kind":if target.kind==Kind::Project{"project"}else{"independent-thread"},
                "target_id":target.id,"output_directory":args.output_directory,"elapsed_seconds":elapsed,"metrics":metrics
            })
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn picker_cancel_stops_automatic_reopen() {
        let mut s = Confirmation::new(None);
        assert!(s.auto_pending);
        assert!(s.open_picker());
        s.picker_result(None);
        assert!(!s.auto_pending);
        assert!(s.output.is_none());
        assert!(!s.open_picker());
    }
    #[test]
    fn cancel_replacement_preserves_previous_selection() {
        let mut s = Confirmation::new(Some("old".into()));
        assert!(!s.auto_pending);
        s.picker_result(None);
        assert_eq!(s.output, Some("old".into()));
        s.picker_result(Some("new".into()));
        assert_eq!(s.output, Some("new".into()));
    }
    #[test]
    fn changing_source_starts_with_cli_output_not_previous_picker() {
        let initial = Some(PathBuf::from("argument"));
        let mut old = Confirmation::new(initial.clone());
        old.picker_result(Some("picker".into()));
        let new = Confirmation::new(initial);
        assert_eq!(new.output, Some("argument".into()));
        let new = Confirmation::new(None);
        assert!(new.output.is_none() && new.auto_pending);
    }
}
