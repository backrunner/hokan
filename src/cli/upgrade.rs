//! `hokan upgrade`：手动检查并安装来自 GitHub Releases 的更新。
//!
//! 设计要点：
//! - 与 AI 向导一样，交互式提示直接读写标准输入/输出（即时回显），不走
//!   `cli::run` 的缓冲输出；脚本化场景用 `--yes` 或 `--check`。
//! - `--auto` 是隐藏的无头模式（由会话启动时的分离子进程调用）：全程零输出，
//!   退出码是唯一信号（0 成功 / 1 失败），错误被静默吞掉。
//! - `--channel` 会把选择持久化到 `[update].channel`（原子写回配置文件），
//!   发生在任何网络请求之前。
//! - 真正的下载/校验/替换全部走 `update::run_upgrade`，手动与自动共用同一路径。

use std::io::{BufRead, IsTerminal, Write};

use crate::{
    config::{Config, ConfigPaths},
    update::{Channel, UpdateError, UpgradeOptions, UpgradeOutcome, UpgradePaths, run_upgrade},
};

/// Parsed flags of the `upgrade` subcommand.
#[derive(Clone, Debug, Default)]
pub struct UpgradeArgs {
    pub check: bool,
    pub channel: Option<String>,
    pub force: bool,
    pub yes: bool,
    pub auto: bool,
}

pub fn run(args: UpgradeArgs) -> crate::Result<()> {
    let paths = ConfigPaths::discover()?;
    if args.auto {
        // 无头分离模式：完全静默，只有退出码；这里没有 debug log 可写，
        // 错误按设计吞掉（下次 TTL 到期会重试）。
        let exe = std::env::current_exe()?;
        let upgrade_paths = UpgradePaths::production(
            exe,
            paths.state_directory.clone(),
            paths.cache_directory.clone(),
        );
        std::process::exit(i32::from(run_auto(&paths, &upgrade_paths)));
    }
    let exe = std::env::current_exe()?;
    let upgrade_paths = UpgradePaths::production(
        exe,
        paths.state_directory.clone(),
        paths.cache_directory.clone(),
    );
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    let mut err = std::io::stderr().lock();
    run_with_io(
        &mut input,
        &mut output,
        &mut err,
        tty,
        &paths,
        &upgrade_paths,
        &args,
    )
}

/// `--auto` 的实现：加载配置、按 `[update]` 运行一次升级；返回进程退出码。
/// 不打印任何内容，也绝不 panic——所有失败都折叠为退出码 1。
fn run_auto(paths: &ConfigPaths, upgrade_paths: &UpgradePaths) -> u8 {
    let Ok(config) = Config::load(&paths.config_file) else {
        return 1;
    };
    let Ok(channel) = Channel::parse(&config.update.channel) else {
        return 1;
    };
    let options = UpgradeOptions {
        channel,
        check_only: false,
        force: false,
        auto: true,
        interval_secs: config.update.interval_secs,
    };
    run_upgrade(&options, upgrade_paths).map_or(1, |_| 0)
}

/// 测试注入点：输入、输出、TTY 状态、路径与 API 端点全部可替换，
/// 使脚本化测试完全不接触终端、真实配置目录与外网。
fn run_with_io(
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    err: &mut dyn Write,
    tty: bool,
    paths: &ConfigPaths,
    upgrade_paths: &UpgradePaths,
    args: &UpgradeArgs,
) -> crate::Result<()> {
    // 先加载配置：解析失败立即中止，避免在损坏的配置上执行升级。
    let mut config = Config::load(&paths.config_file)?;

    // 显式指定的渠道先持久化到配置文件，再执行检查/升级。
    let channel = match &args.channel {
        Some(value) => {
            let channel = Channel::parse(value).map_err(|error| {
                crate::Error::Config(format!(
                    "未知更新渠道 {value}（{}）；可选 stable 或 beta",
                    error.code()
                ))
            })?;
            config.update.channel = channel.as_str().to_owned();
            config.write_atomic(&paths.config_file)?;
            writeln!(output, "更新渠道已保存：{channel}")?;
            channel
        }
        None => Channel::parse(&config.update.channel)
            .map_err(|error| crate::Error::Config(error.to_string()))?,
    };

    if !tty && !args.yes && !args.check {
        return Err(crate::Error::Config(
            "`hokan upgrade` 需要在终端中交互确认；脚本化升级请使用 `hokan upgrade --yes`，\
             只检查新版本请使用 `hokan upgrade --check`"
                .into(),
        ));
    }

    let options = UpgradeOptions {
        channel,
        check_only: true,
        force: args.force,
        auto: false,
        interval_secs: config.update.interval_secs,
    };
    // 先只做检查，拿到“当前 → 最新”供用户确认；确认后再走完整升级。
    let checked = run_upgrade(&options, upgrade_paths).map_err(update_failure)?;
    let UpgradeOutcome::Checked { current, latest } = checked else {
        return Err(crate::Error::Config(
            "内部错误：升级检查返回了非预期结果".into(),
        ));
    };
    writeln!(output, "当前 v{current} → 最新 v{latest} [{channel}]")?;

    if args.check {
        if latest > current {
            writeln!(output, "可升级：运行 hokan upgrade 安装")?;
        } else {
            writeln!(output, "已是最新版本")?;
        }
        return Ok(());
    }
    if latest < current || (latest == current && !args.force) {
        writeln!(output, "已是最新版本 v{current}")?;
        return Ok(());
    }

    if !args.yes && !confirm(input, output, err)? {
        writeln!(output, "已取消")?;
        return Ok(());
    }

    writeln!(output, "正在下载并校验 v{latest} …")?;
    let outcome = run_upgrade(
        &UpgradeOptions {
            check_only: false,
            ..options
        },
        upgrade_paths,
    )
    .map_err(update_failure)?;
    match outcome {
        UpgradeOutcome::Upgraded { from, to } => {
            writeln!(output, "SHA256 校验与冒烟测试通过")?;
            writeln!(output, "升级完成：v{from} → v{to}，下次启动生效")?;
            Ok(())
        }
        UpgradeOutcome::AlreadyCurrent { version } => {
            writeln!(output, "已是最新版本 v{version}")?;
            Ok(())
        }
        UpgradeOutcome::NotWritable { path } => {
            writeln!(err, "当前安装路径不可写，未做任何改动。")?;
            writeln!(
                err,
                "请使用你的包管理器升级（路径不可写: {}）",
                path.display()
            )?;
            Err(crate::Error::Config(format!(
                "升级未完成：可执行文件路径不可写（{}）",
                path.display()
            )))
        }
        UpgradeOutcome::Checked { .. } => Err(crate::Error::Config(
            "内部错误：升级返回了非预期结果".into(),
        )),
    }
}

/// `[Y/n]` 确认：回车或 y 继续，n 取消，EOF（Ctrl-D）视为取消。
fn confirm(
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    err: &mut dyn Write,
) -> crate::Result<bool> {
    loop {
        write!(output, "确认升级？[Y/n] ")?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(false);
        }
        match line.trim() {
            "" | "y" | "Y" => return Ok(true),
            "n" | "N" => return Ok(false),
            _ => writeln!(err, "请输入 y 或 n")?,
        }
    }
}

fn update_failure(error: UpdateError) -> crate::Error {
    crate::Error::Config(format!("升级失败（{}）：{error}", error.code()))
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Cursor, path::Path};

    use sha2::{Digest, Sha256};

    use super::*;
    use crate::update::test_support::{
        archive_asset, build_archive, json_reply, raw_reply, release_json, sha256sums_for,
        spawn_server, write_stub_binary,
    };

    fn test_paths(root: &Path) -> ConfigPaths {
        ConfigPaths {
            config_file: root.join("config.toml"),
            credentials_file: root.join("credentials.toml"),
            specs_directory: root.join("specs"),
            state_directory: root.join("state"),
            cache_directory: root.join("cache"),
        }
    }

    fn make_upgrade_paths(root: &Path, api_base: &str) -> UpgradePaths {
        let bin = root.join("bin");
        fs::create_dir_all(&bin).expect("bin dir");
        let current_exe = bin.join("hokan");
        write_stub_binary(
            &current_exe,
            &format!("#!/bin/sh\necho hokan {}\n", env!("CARGO_PKG_VERSION")),
        );
        UpgradePaths {
            current_exe,
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
            api_base: api_base.to_owned(),
            repo: "backrunner/hokan".to_owned(),
        }
    }

    /// Serves the four requests of a full confirmed upgrade: the pre-check,
    /// the real run's release lookup, the archive, and its SHA256SUMS.
    fn serve_full_upgrade(version: &str, requests: usize) -> (String, std::thread::JoinHandle<()>) {
        let archive = build_archive(&format!("#!/bin/sh\necho hokan {version}\n"));
        let sums = sha256sums_for(&[(
            &format!("{:x}", Sha256::digest(&archive)),
            &archive_asset(version),
        )])
        .into_bytes();
        let archive_name = archive_asset(version);
        let tag = format!("v{version}");
        spawn_server(requests, move |path| {
            if path.starts_with("/repos/") {
                let release = release_json(&tag, &[archive_name.clone(), "SHA256SUMS".to_owned()]);
                // The beta channel queries the release list (an array);
                // stable queries `/releases/latest` (a single object).
                let body = if path.contains("releases?") {
                    serde_json::json!([release])
                } else {
                    release
                };
                json_reply("200 OK", body)
            } else if path == format!("/download/{archive_name}") {
                raw_reply("200 OK", archive.clone())
            } else if path == "/download/SHA256SUMS" {
                raw_reply("200 OK", sums.clone())
            } else {
                raw_reply("404 Not Found", Vec::new())
            }
        })
    }

    fn run_scripted(
        stdin: &str,
        tty: bool,
        paths: &ConfigPaths,
        upgrade_paths: &UpgradePaths,
        args: &UpgradeArgs,
    ) -> (crate::Result<()>, String, String) {
        let mut input = Cursor::new(stdin.as_bytes().to_vec());
        let mut output = Vec::new();
        let mut err = Vec::new();
        let result = run_with_io(
            &mut input,
            &mut output,
            &mut err,
            tty,
            paths,
            upgrade_paths,
            args,
        );
        (
            result,
            String::from_utf8(output).expect("output UTF-8"),
            String::from_utf8(err).expect("err UTF-8"),
        )
    }

    #[test]
    fn check_reports_availability_without_installing() {
        let root = tempfile::tempdir().expect("tempdir");
        let (base, join) = serve_full_upgrade("9.9.9", 1);
        let paths = test_paths(root.path());
        let upgrade_paths = make_upgrade_paths(root.path(), &base);
        let exe_before = fs::read(&upgrade_paths.current_exe).expect("exe");

        let args = UpgradeArgs {
            check: true,
            ..UpgradeArgs::default()
        };
        let (result, output, _) = run_scripted("", false, &paths, &upgrade_paths, &args);
        result.expect("check run");
        assert!(
            output.contains(&format!(
                "当前 v{} → 最新 v9.9.9 [stable]",
                env!("CARGO_PKG_VERSION")
            )),
            "{output}"
        );
        assert!(output.contains("可升级"), "{output}");
        join.join().expect("server thread");
        // --check 不下载、不改动可执行文件。
        assert_eq!(
            fs::read(&upgrade_paths.current_exe).expect("exe"),
            exe_before
        );
        assert!(!upgrade_paths.cache_dir.join("downloads").exists());
    }

    #[test]
    fn interactive_confirm_yes_upgrades() {
        let root = tempfile::tempdir().expect("tempdir");
        let (base, join) = serve_full_upgrade("9.9.9", 4);
        let paths = test_paths(root.path());
        let upgrade_paths = make_upgrade_paths(root.path(), &base);

        let (result, output, _) =
            run_scripted("y\n", true, &paths, &upgrade_paths, &UpgradeArgs::default());
        result.expect("upgrade run");
        assert!(output.contains("确认升级？[Y/n]"), "{output}");
        assert!(output.contains("SHA256 校验与冒烟测试通过"), "{output}");
        assert!(
            output.contains(&format!(
                "升级完成：v{} → v9.9.9，下次启动生效",
                env!("CARGO_PKG_VERSION")
            )),
            "{output}"
        );
        join.join().expect("server thread");
        let new_exe = fs::read_to_string(&upgrade_paths.current_exe).expect("new exe");
        assert!(new_exe.contains("9.9.9"), "{new_exe}");
        // 单份备份保留旧版本。
        let backup = fs::read_to_string(root.path().join("bin/hokan.bak")).expect("backup");
        assert!(backup.contains(env!("CARGO_PKG_VERSION")), "{backup}");
    }

    #[test]
    fn interactive_decline_or_eof_cancels() {
        for stdin in ["n\n", ""] {
            let root = tempfile::tempdir().expect("tempdir");
            let (base, join) = serve_full_upgrade("9.9.9", 1);
            let paths = test_paths(root.path());
            let upgrade_paths = make_upgrade_paths(root.path(), &base);
            let exe_before = fs::read(&upgrade_paths.current_exe).expect("exe");

            let (result, output, _) =
                run_scripted(stdin, true, &paths, &upgrade_paths, &UpgradeArgs::default());
            result.expect("declined run");
            assert!(output.contains("已取消"), "{output}");
            join.join().expect("server thread");
            assert_eq!(
                fs::read(&upgrade_paths.current_exe).expect("exe"),
                exe_before
            );
        }
    }

    #[test]
    fn channel_flag_persists_before_checking() {
        let root = tempfile::tempdir().expect("tempdir");
        // beta 渠道：release 列表 + latest 共两次请求。
        let (base, join) = serve_full_upgrade("9.9.9-beta.1", 2);
        let paths = test_paths(root.path());
        let upgrade_paths = make_upgrade_paths(root.path(), &base);

        let args = UpgradeArgs {
            check: true,
            channel: Some("beta".to_owned()),
            ..UpgradeArgs::default()
        };
        let (result, output, _) = run_scripted("", false, &paths, &upgrade_paths, &args);
        result.expect("channel run");
        assert!(output.contains("更新渠道已保存：beta"), "{output}");
        assert!(output.contains("[beta]"), "{output}");
        join.join().expect("server thread");

        let saved = fs::read_to_string(&paths.config_file).expect("saved config");
        assert!(saved.contains("channel = \"beta\""), "{saved}");
        let reloaded = Config::load(&paths.config_file).expect("reload config");
        assert_eq!(reloaded.update.channel, "beta");
    }

    #[test]
    fn invalid_channel_is_rejected_before_any_network() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(root.path());
        // 无服务器：任何网络请求都会失败；这里必须在联网前就报错。
        let upgrade_paths = make_upgrade_paths(root.path(), "http://127.0.0.1:1");
        let args = UpgradeArgs {
            check: true,
            channel: Some("nightly".to_owned()),
            ..UpgradeArgs::default()
        };
        let (result, _, _) = run_scripted("", false, &paths, &upgrade_paths, &args);
        let error = result.expect_err("invalid channel must fail");
        assert!(error.to_string().contains("stable 或 beta"), "{error}");
        assert!(!paths.config_file.exists());
    }

    #[test]
    fn non_tty_without_yes_or_check_errors_with_scripted_hint() {
        let root = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(root.path());
        let upgrade_paths = make_upgrade_paths(root.path(), "http://127.0.0.1:1");
        let (result, _, _) =
            run_scripted("", false, &paths, &upgrade_paths, &UpgradeArgs::default());
        let error = result.expect_err("non-TTY must fail");
        let detail = error.to_string();
        assert!(detail.contains("--yes"), "{detail}");
        assert!(detail.contains("--check"), "{detail}");
    }

    #[cfg(unix)]
    #[test]
    fn not_writable_exe_prints_package_manager_advice() {
        use std::os::unix::fs::PermissionsExt;
        if nix::unistd::geteuid().is_root() {
            // Root ignores permission bits; the probe would succeed.
            return;
        }
        let root = tempfile::tempdir().expect("tempdir");
        let (base, join) = serve_full_upgrade("9.9.9", 4);
        let paths = test_paths(root.path());
        let upgrade_paths = make_upgrade_paths(root.path(), &base);
        let bin = root.path().join("bin");
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o555)).expect("read-only bin");

        let args = UpgradeArgs {
            yes: true,
            ..UpgradeArgs::default()
        };
        let (result, _, err) = run_scripted("", true, &paths, &upgrade_paths, &args);
        let error = result.expect_err("not writable must fail");
        assert!(error.to_string().contains("不可写"), "{error}");
        assert!(err.contains("请使用你的包管理器升级"), "{err}");
        join.join().expect("server thread");

        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).expect("restore bin");
        // 未做任何替换、未留备份。
        let exe = fs::read_to_string(&upgrade_paths.current_exe).expect("exe");
        assert!(exe.contains("0.1.0"), "{exe}");
        assert!(!root.path().join("bin/hokan.bak").exists());
    }

    #[test]
    fn auto_mode_reports_only_via_exit_code() {
        // 成功路径：新鲜缓存短路，零网络。
        let root = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(root.path().join("state")).expect("state dir");
        fs::write(
            root.path().join("state/update-check.json"),
            format!(
                "{{\"last_check_epoch\":{},\"channel\":\"stable\",\"latest_known\":\"0.0.1\"}}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_secs())
                    .unwrap_or(0)
            ),
        )
        .expect("seed cache");
        let paths = test_paths(root.path());
        let upgrade_paths = make_upgrade_paths(root.path(), "http://127.0.0.1:1");
        assert_eq!(run_auto(&paths, &upgrade_paths), 0);

        // 失败路径：服务器立即 404，优雅退出 1（不 panic、无输出）。
        let root = tempfile::tempdir().expect("tempdir");
        let (base, join) = spawn_server(1, |_| raw_reply("404 Not Found", Vec::new()));
        let paths = test_paths(root.path());
        let failing_paths = make_upgrade_paths(root.path(), &base);
        assert_eq!(run_auto(&paths, &failing_paths), 1);
        join.join().expect("server thread");
    }

    #[test]
    fn graceful_error_when_api_404s() {
        let root = tempfile::tempdir().expect("tempdir");
        let (base, join) = spawn_server(1, |_| raw_reply("404 Not Found", Vec::new()));
        let paths = test_paths(root.path());
        let upgrade_paths = make_upgrade_paths(root.path(), &base);
        let args = UpgradeArgs {
            check: true,
            ..UpgradeArgs::default()
        };
        let (result, _, _) = run_scripted("", false, &paths, &upgrade_paths, &args);
        let error = result.expect_err("404 must fail gracefully");
        let detail = error.to_string();
        assert!(detail.contains("HK-UPD-HTTP"), "{detail}");
        join.join().expect("server thread");
    }
}
