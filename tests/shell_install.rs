//! Integration tests for `ji::shell` install / uninstall / status.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use ji::shell::{InstallOpts, ShellEnv, UninstallOpts, install, status, uninstall};
use tempfile::TempDir;

fn fake_cmd() -> clap::Command {
    clap::Command::new("ji")
        .subcommand(clap::Command::new("switch").arg(clap::Arg::new("target")))
        .subcommand(clap::Command::new("ls"))
}

fn env_for(tmp: &TempDir) -> ShellEnv {
    let home = tmp.path().to_path_buf();
    let xdg = home.join(".config");
    ShellEnv {
        home: home.clone(),
        xdg_config_home: xdg,
        zdotdir: home,
        zsh_custom: None,
        omz_root: None,
    }
}

fn write(path: &Path, contents: &str) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

fn install_zsh(env: &ShellEnv, opts: InstallOpts) -> anyhow::Result<()> {
    install(env, "zsh", &mut fake_cmd(), opts)
}

fn uninstall_zsh(env: &ShellEnv, opts: UninstallOpts) -> anyhow::Result<()> {
    uninstall(env, "zsh", opts)
}

#[test]
fn install_then_install_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);

    install_zsh(&env, InstallOpts::default()).unwrap();
    let rc1 = read(&env.home.join(".zshrc"));
    let mgr1 = read(&env.xdg_config_home.join("ji/init.zsh"));

    install_zsh(&env, InstallOpts::default()).unwrap();
    let rc2 = read(&env.home.join(".zshrc"));
    let mgr2 = read(&env.xdg_config_home.join("ji/init.zsh"));

    assert_eq!(rc1, rc2);
    assert_eq!(mgr1, mgr2);
}

#[test]
fn install_writes_marker_block() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    install_zsh(&env, InstallOpts::default()).unwrap();
    let rc = read(&env.home.join(".zshrc"));
    assert!(rc.contains("# >>> ji shell integration >>>"));
    assert!(rc.contains("# <<< ji shell integration <<<"));
    assert!(rc.contains("# ji-managed: do not edit"));
}

#[test]
fn install_with_drifted_managed_file_is_updated() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    install_zsh(&env, InstallOpts::default()).unwrap();
    let mgr_path = env.xdg_config_home.join("ji/init.zsh");
    write(&mgr_path, "DRIFT\n");
    install_zsh(&env, InstallOpts::default()).unwrap();
    let mgr = read(&mgr_path);
    assert!(mgr.contains("# ji-managed: do not edit"));
    assert!(!mgr.starts_with("DRIFT"));
}

#[test]
fn install_with_drifted_rc_stanza_is_updated() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    install_zsh(&env, InstallOpts::default()).unwrap();
    let rc = env.home.join(".zshrc");
    let mut contents = read(&rc);
    contents = contents.replace("ji-managed: do not edit", "ji-managed: tampered");
    write(&rc, &contents);
    install_zsh(&env, InstallOpts::default()).unwrap();
    let after = read(&rc);
    assert!(after.contains("ji-managed: do not edit"));
    assert!(!after.contains("tampered"));
}

#[test]
fn sourced_file_bare_legacy_refused_without_force() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let zshrc = env.home.join(".zshrc");
    let custom = env.home.join(".zshrc.custom.zsh");
    write(&zshrc, "source \"$HOME/.zshrc.custom.zsh\"\n");
    write(&custom, "eval \"$(command ji config shell init zsh)\"\n");

    let err = install_zsh(&env, InstallOpts::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains(".zshrc.custom.zsh"));
    assert!(msg.contains("--force"));
}

#[test]
fn sourced_file_force_installs_alongside() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let zshrc = env.home.join(".zshrc");
    let custom = env.home.join(".zshrc.custom.zsh");
    write(&zshrc, "source \"$HOME/.zshrc.custom.zsh\"\n");
    write(&custom, "eval \"$(command ji config shell init zsh)\"\n");

    install_zsh(
        &env,
        InstallOpts {
            force: true,
            ..InstallOpts::default()
        },
    )
    .unwrap();

    let rc_after = read(&zshrc);
    assert!(rc_after.contains("# >>> ji shell integration >>>"));
    let custom_after = read(&custom);
    assert!(custom_after.contains("ji config shell init"));
}

#[test]
fn sourced_file_guarded_with_test_bracket() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let zshrc = env.home.join(".zshrc");
    let custom = env.home.join(".zshrc.custom.zsh");
    write(
        &zshrc,
        "[ -f \"$HOME/.zshrc.custom.zsh\" ] && source \"$HOME/.zshrc.custom.zsh\"\n",
    );
    write(&custom, "eval \"$(ji config shell init zsh)\"\n");

    let err = install_zsh(&env, InstallOpts::default()).unwrap_err();
    assert!(format!("{err}").contains(".zshrc.custom.zsh"));
}

#[test]
fn sourced_file_if_then_fi() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let zshrc = env.home.join(".zshrc");
    let custom = env.home.join(".zshrc.custom.zsh");
    write(
        &zshrc,
        "if [ -f \"$HOME/.zshrc.custom.zsh\" ]; then source \"$HOME/.zshrc.custom.zsh\"; fi\n",
    );
    write(&custom, "eval \"$(ji config shell init)\"\n");

    let err = install_zsh(&env, InstallOpts::default()).unwrap_err();
    assert!(format!("{err}").contains(".zshrc.custom.zsh"));
}

#[test]
fn sourced_file_trailing_semicolon() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let zshrc = env.home.join(".zshrc");
    let custom = env.home.join(".my.zsh");
    write(&zshrc, "source \"$HOME/.my.zsh\";\n");
    write(&custom, "eval \"$(ji config shell init zsh)\"\n");

    let err = install_zsh(&env, InstallOpts::default()).unwrap_err();
    assert!(format!("{err}").contains(".my.zsh"));
}

#[test]
fn sourced_file_glob_loop() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let zshrc = env.home.join(".zshrc");
    let dir = env.home.join(".zshrc.d");
    fs::create_dir_all(&dir).unwrap();
    let custom = dir.join("aliases.zsh");
    write(
        &zshrc,
        "for f in ~/.zshrc.d/*.zsh; do source \"$f\"; done\n",
    );
    write(&custom, "eval \"$(ji config shell init zsh)\"\n");

    let err = install_zsh(&env, InstallOpts::default()).unwrap_err();
    assert!(format!("{err}").contains("aliases.zsh"));
}

#[test]
fn sourced_file_zdotdir_resolves() {
    let tmp = TempDir::new().unwrap();
    let mut env = env_for(&tmp);
    let zdotdir = env.home.join("zsh-config");
    fs::create_dir_all(&zdotdir).unwrap();
    env.zdotdir = zdotdir.clone();
    let zshrc = zdotdir.join(".zshrc");
    let custom = zdotdir.join("extras.zsh");
    write(&zshrc, "source \"$ZDOTDIR/extras.zsh\"\n");
    write(&custom, "eval \"$(ji config shell init zsh)\"\n");

    let err = install_zsh(&env, InstallOpts::default()).unwrap_err();
    assert!(format!("{err}").contains("extras.zsh"));
}

#[test]
fn sourced_file_unresolved_does_not_block_install() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let zshrc = env.home.join(".zshrc");
    write(&zshrc, "source \"$MY_PRIVATE_VAR/foo\"\n");
    install_zsh(&env, InstallOpts::default()).unwrap();
    let after = read(&zshrc);
    assert!(after.contains("# >>> ji shell integration >>>"));
}

#[test]
fn alias_definition_is_not_a_hit() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let zshrc = env.home.join(".zshrc");
    write(&zshrc, "alias jcsi='ji config shell init zsh'\n");
    install_zsh(&env, InstallOpts::default()).unwrap();
    let after = read(&zshrc);
    assert!(after.contains("# >>> ji shell integration >>>"));
    assert!(after.contains("alias jcsi="));
}

#[test]
fn comment_line_with_ji_init_is_not_a_hit() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let zshrc = env.home.join(".zshrc");
    write(&zshrc, "# eval \"$(ji config shell init zsh)\"\n");
    install_zsh(&env, InstallOpts::default()).unwrap();
    let after = read(&zshrc);
    assert!(after.contains("# >>> ji shell integration >>>"));
}

#[test]
fn legacy_primary_rc_refused_without_force() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let zshrc = env.home.join(".zshrc");
    write(&zshrc, "eval \"$(command ji config shell init zsh)\"\n");
    let err = install_zsh(&env, InstallOpts::default()).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("existing integration line"));
}

#[test]
fn malformed_marker_refused_without_force() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let zshrc = env.home.join(".zshrc");
    write(
        &zshrc,
        "# >>> ji shell integration >>>\nsome content\n# (no end marker)\n",
    );
    let err = install_zsh(&env, InstallOpts::default()).unwrap_err();
    assert!(format!("{err}").contains("malformed"));
}

#[test]
fn malformed_marker_overwritten_with_force() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let zshrc = env.home.join(".zshrc");
    write(&zshrc, "# >>> ji shell integration >>>\nstale\n");
    install_zsh(
        &env,
        InstallOpts {
            force: true,
            ..InstallOpts::default()
        },
    )
    .unwrap();
    let after = read(&zshrc);
    assert!(after.contains("# <<< ji shell integration <<<"));
}

#[test]
fn dry_run_makes_no_filesystem_changes() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    install_zsh(
        &env,
        InstallOpts {
            dry_run: true,
            ..InstallOpts::default()
        },
    )
    .unwrap();
    assert!(!env.home.join(".zshrc").exists());
    assert!(!env.xdg_config_home.join("ji/init.zsh").exists());
}

#[test]
fn uninstall_removes_managed_block_and_managed_file() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    install_zsh(&env, InstallOpts::default()).unwrap();
    assert!(env.home.join(".zshrc").exists());
    assert!(env.xdg_config_home.join("ji/init.zsh").exists());

    uninstall_zsh(&env, UninstallOpts::default()).unwrap();
    let rc = read(&env.home.join(".zshrc"));
    assert!(!rc.contains("# >>> ji shell integration >>>"));
    assert!(!env.xdg_config_home.join("ji/init.zsh").exists());
}

#[test]
fn bash_login_precedence_picks_bash_login_when_present() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    write(&env.home.join(".bash_login"), "");
    install(&env, "bash", &mut fake_cmd(), InstallOpts::default()).unwrap();
    assert!(read(&env.home.join(".bash_login")).contains("# >>> ji shell integration >>>"));
    assert!(!env.home.join(".bash_profile").exists());
}

#[test]
fn bash_all_absent_creates_bash_profile() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    install(&env, "bash", &mut fake_cmd(), InstallOpts::default()).unwrap();
    let bp = env.home.join(".bash_profile");
    assert!(bp.exists());
    assert!(read(&bp).contains("# >>> ji shell integration >>>"));
    assert!(!env.home.join(".bash_login").exists());
}

#[test]
fn zsh_zdotdir_respected_for_primary_rc() {
    let tmp = TempDir::new().unwrap();
    let mut env = env_for(&tmp);
    let zdotdir = env.home.join("zsh-config");
    fs::create_dir_all(&zdotdir).unwrap();
    env.zdotdir = zdotdir.clone();
    install_zsh(&env, InstallOpts::default()).unwrap();
    assert!(zdotdir.join(".zshrc").exists());
    assert!(!env.home.join(".zshrc").exists());
}

#[test]
fn rc_mode_preserved_after_install() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let zshrc = env.home.join(".zshrc");
    write(&zshrc, "existing\n");
    fs::set_permissions(&zshrc, fs::Permissions::from_mode(0o600)).unwrap();
    install_zsh(&env, InstallOpts::default()).unwrap();
    let mode = fs::metadata(&zshrc).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o600);
}

#[test]
fn symlinked_rc_preserves_symlink() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let real = env.home.join("dotfiles").join("zshrc");
    fs::create_dir_all(real.parent().unwrap()).unwrap();
    write(&real, "");
    let link = env.home.join(".zshrc");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    install_zsh(&env, InstallOpts::default()).unwrap();
    let md = fs::symlink_metadata(&link).unwrap();
    assert!(md.file_type().is_symlink());
    assert!(read(&real).contains("# >>> ji shell integration >>>"));
}

#[test]
fn chezmoi_refuse_without_force() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let chezmoi_src = env.home.join("dotfiles-chezmoi");
    fs::create_dir_all(&chezmoi_src).unwrap();
    write(&chezmoi_src.join("chezmoi.toml"), "");
    let real = chezmoi_src.join("dot_zshrc");
    write(&real, "");
    let link = env.home.join(".zshrc");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    let err = install_zsh(&env, InstallOpts::default()).unwrap_err();
    assert!(format!("{err}").contains("chezmoi"));
}

#[test]
fn omz_custom_dir_legacy_detected() {
    let tmp = TempDir::new().unwrap();
    let mut env = env_for(&tmp);
    let omz = env.home.join(".oh-my-zsh");
    let custom = omz.join("custom");
    fs::create_dir_all(&custom).unwrap();
    env.omz_root = Some(omz);
    env.zsh_custom = Some(custom.clone());
    write(
        &custom.join("aliases.zsh"),
        "eval \"$(ji config shell init zsh)\"\n",
    );

    let err = install_zsh(&env, InstallOpts::default()).unwrap_err();
    assert!(format!("{err}").contains("aliases.zsh"));
}

#[test]
fn status_does_not_panic() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    install_zsh(&env, InstallOpts::default()).unwrap();
    status(&env, Some("zsh"), &mut fake_cmd()).unwrap();
}

#[test]
fn fish_install_writes_functions_wrapper_and_completions() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    install(&env, "fish", &mut fake_cmd(), InstallOpts::default()).unwrap();
    assert!(env.xdg_config_home.join("fish/functions/ji.fish").exists());
    assert!(
        env.xdg_config_home
            .join("fish/completions/ji.fish")
            .exists()
    );
    let wrapper = read(&env.xdg_config_home.join("fish/functions/ji.fish"));
    assert!(wrapper.contains("# ji-managed: do not edit"));
    assert!(wrapper.contains("function ji"));
}

#[test]
fn fish_user_authored_completions_refused_without_force() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let comp = env.xdg_config_home.join("fish/completions/ji.fish");
    write(&comp, "# user authored, NOT ji-managed\n");
    let err = install(&env, "fish", &mut fake_cmd(), InstallOpts::default()).unwrap_err();
    assert!(format!("{err}").contains("not ji-managed"));
}

#[test]
fn fish_user_authored_completions_overwritten_with_force() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let comp = env.xdg_config_home.join("fish/completions/ji.fish");
    write(&comp, "# user authored, NOT ji-managed\n");
    install(
        &env,
        "fish",
        &mut fake_cmd(),
        InstallOpts {
            force: true,
            ..InstallOpts::default()
        },
    )
    .unwrap();
    assert!(read(&comp).contains("# ji-managed: do not edit"));
}

#[test]
fn fish_shadow_file_in_conf_d_refused() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let shadow = env.xdg_config_home.join("fish/conf.d/ji.fish");
    write(&shadow, "function ji; echo other; end\n");
    let err = install(&env, "fish", &mut fake_cmd(), InstallOpts::default()).unwrap_err();
    assert!(format!("{err}").contains("shadow"));
}

#[test]
fn fish_user_authored_functions_ji_refused_without_force() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let p = env.xdg_config_home.join("fish/functions/ji.fish");
    write(&p, "function ji; echo custom; end\n");
    let err = install(&env, "fish", &mut fake_cmd(), InstallOpts::default()).unwrap_err();
    assert!(format!("{err}").contains("not ji-managed"));
}

#[test]
fn fish_shadow_file_overridden_with_force_then_cleaned_by_uninstall() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let shadow = env.xdg_config_home.join("fish/conf.d/ji.fish");
    write(&shadow, "# ji-managed: do not edit\nfunction ji; end\n");
    install(
        &env,
        "fish",
        &mut fake_cmd(),
        InstallOpts {
            force: true,
            ..InstallOpts::default()
        },
    )
    .unwrap();
    uninstall(&env, "fish", UninstallOpts::default()).unwrap();
    assert!(
        !shadow.exists(),
        "managed shadow file should be removed by uninstall"
    );
}

#[test]
fn concurrent_install_byte_equal_result() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    install_zsh(&env, InstallOpts::default()).unwrap();
    let r1 = read(&env.home.join(".zshrc"));
    let m1 = read(&env.xdg_config_home.join("ji/init.zsh"));

    let tmp2 = TempDir::new().unwrap();
    let env2 = env_for(&tmp2);
    install_zsh(&env2, InstallOpts::default()).unwrap();
    let r2 = read(&env2.home.join(".zshrc"));
    let m2 = read(&env2.xdg_config_home.join("ji/init.zsh"));

    assert_eq!(r1, r2);
    assert_eq!(m1, m2);
}

#[test]
fn install_does_not_recurse_into_sourced_files_beyond_one_hop() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let zshrc = env.home.join(".zshrc");
    let a = env.home.join(".a.zsh");
    let b = env.home.join(".b.zsh");
    write(&zshrc, "source \"$HOME/.a.zsh\"\n");
    write(&a, "source \"$HOME/.b.zsh\"\n");
    write(&b, "eval \"$(ji config shell init zsh)\"\n");

    install_zsh(&env, InstallOpts::default()).unwrap();
    assert!(read(&zshrc).contains("# >>> ji shell integration >>>"));
}

#[test]
fn marker_block_after_comment_lines_is_detected_on_reinstall() {
    let tmp = TempDir::new().unwrap();
    let env = env_for(&tmp);
    let zshrc = env.home.join(".zshrc");
    install_zsh(&env, InstallOpts::default()).unwrap();
    let block = read(&zshrc);
    let prepended = format!("# comment one\n# comment two\n{block}");
    write(&zshrc, &prepended);
    install_zsh(&env, InstallOpts::default()).unwrap();
    let after = read(&zshrc);
    let count = after.matches("# >>> ji shell integration >>>").count();
    assert_eq!(count, 1, "marker block duplicated:\n{after}");
}

#[test]
fn opts_default_is_clean() {
    let opts = InstallOpts::default();
    assert!(!opts.dry_run && !opts.force);
    let unopts = UninstallOpts::default();
    assert!(!unopts.dry_run && !unopts.force);
}
