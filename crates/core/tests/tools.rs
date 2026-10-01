//! Integration tests for the maintenance tools. No maintenance tool is ever started: the
//! launcher runs only `cmd.exe` built-ins (`echo`, `exit`) and `ping.exe -n N 127.0.0.1`,
//! and plans are read with a launcher that fails the test if it is asked to start anything.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use optimizer_core::tools::launch::DETACH_REFUSED;
use optimizer_core::tools::{
    self, CommandSpec, DetachPolicy, Environment, Launched, Launcher, RunningProcess, StoreHealth,
    SystemLauncher, ToolId, ToolRequest, ToolRunner,
};
use optimizer_core::win::console_text::{OutputDecoder, OutputEvent, TextEncoding};
use optimizer_core::win::paths::system_dir;
use optimizer_core::win::volume::fixed_volumes;

fn system_program(name: &str) -> PathBuf {
    system_dir().unwrap().join(name)
}

fn spec(program: &str, args: &[&str], detach: DetachPolicy) -> CommandSpec {
    CommandSpec {
        program: system_program(program),
        args: args.iter().map(|a| a.to_string()).collect(),
        detach,
    }
}

/// Starts `command` with its output going to `out.raw` in a temporary folder.
fn launch(command: &CommandSpec) -> (tempfile::TempDir, PathBuf, Launched) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.raw");
    let output = File::create(&path).unwrap();
    let launched = SystemLauncher::new().launch(command, output).unwrap();
    (dir, path, launched)
}

fn wait_exit(process: &mut dyn RunningProcess, timeout: Duration) -> i32 {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(code) = process.try_wait().unwrap() {
            return code;
        }
        assert!(Instant::now() < deadline, "the process did not end");
        thread::sleep(Duration::from_millis(10));
    }
}

fn decode(path: &Path, expected: TextEncoding) -> (Vec<OutputEvent>, Option<TextEncoding>) {
    let mut decoder = OutputDecoder::new(expected);
    let mut events = Vec::new();
    decoder.push(&fs::read(path).unwrap(), &mut events);
    decoder.finish(&mut events);
    (events, decoder.encoding())
}

#[test]
fn system_launcher_captures_utf16_output() {
    let command = spec(
        "cmd.exe",
        &["/d", "/u", "/c", "echo", "PCOptimizer-Probe"],
        DetachPolicy::None,
    );
    let (_dir, path, mut launched) = launch(&command);
    assert!(launched.process.id() > 0);
    assert_eq!(
        wait_exit(launched.process.as_mut(), Duration::from_secs(30)),
        0
    );
    drop(launched);

    // Nothing says UTF-16 up front; the NUL bytes of the text identify it.
    let oem = TextEncoding::CodePage(optimizer_core::win::console_text::oem_code_page());
    let (events, encoding) = decode(&path, oem);
    assert_eq!(encoding, Some(TextEncoding::Utf16Le));
    assert_eq!(events, [OutputEvent::Line("PCOptimizer-Probe".to_string())]);
}

#[test]
fn system_launcher_reports_exit_codes() {
    for (code, text) in [(3, "3"), (-2_146_498_529, "-2146498529")] {
        let command = spec("cmd.exe", &["/d", "/c", "exit", text], DetachPolicy::None);
        let (_dir, _path, mut launched) = launch(&command);
        let exit = wait_exit(launched.process.as_mut(), Duration::from_secs(30));
        assert_eq!(exit, code);
        assert_eq!(tools::exit_code_hex(exit), format!("0x{:08X}", code as u32));
    }
}

#[test]
fn system_launcher_kill_stops_a_process() {
    let command = spec("ping.exe", &["-n", "30", "127.0.0.1"], DetachPolicy::None);
    let (_dir, path, mut launched) = launch(&command);
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        launched.process.try_wait().unwrap(),
        None,
        "ping ended early"
    );
    assert!(
        launched.process.kill().unwrap(),
        "stopping a running process reports that it stopped it"
    );
    assert_eq!(
        wait_exit(launched.process.as_mut(), Duration::from_secs(10)),
        1
    );
    // Stopping a process that already ended succeeds and reports that it did nothing.
    assert!(!launched.process.kill().unwrap());
    drop(launched);

    // So does stopping one that ended on its own, with its own exit code kept.
    let command = spec("cmd.exe", &["/d", "/c", "exit", "0"], DetachPolicy::None);
    let (_dir, _path, mut ended) = launch(&command);
    assert_eq!(
        wait_exit(ended.process.as_mut(), Duration::from_secs(30)),
        0
    );
    assert!(!ended.process.kill().unwrap());
    assert_eq!(ended.process.try_wait().unwrap(), Some(0));
    let oem = TextEncoding::CodePage(optimizer_core::win::console_text::oem_code_page());
    let (events, _) = decode(&path, oem);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, OutputEvent::Line(text) if text.contains("127.0.0.1"))),
        "{events:?}"
    );
}

#[test]
fn detached_flag_is_reported() {
    let quick = |detach| spec("cmd.exe", &["/d", "/c", "exit", "0"], detach);
    let (_dir, _path, mut inside) = launch(&quick(DetachPolicy::None));
    assert!(!inside.detached, "a job that can be stopped stays inside");
    wait_exit(inside.process.as_mut(), Duration::from_secs(30));

    let (_dir, _path, mut preferred) = launch(&quick(DetachPolicy::Prefer));
    wait_exit(preferred.process.as_mut(), Duration::from_secs(30));

    // Whether breaking away works depends on the job this test runs in (if any); the two
    // policies must agree about it.
    let dir = tempfile::tempdir().unwrap();
    let output = File::create(dir.path().join("out.raw")).unwrap();
    match SystemLauncher::new().launch(&quick(DetachPolicy::Require), output) {
        Ok(mut required) => {
            assert!(required.detached);
            assert!(
                preferred.detached,
                "Prefer must break away when Require can"
            );
            wait_exit(required.process.as_mut(), Duration::from_secs(30));
        }
        Err(e) => {
            assert_eq!(e.to_string(), DETACH_REFUSED);
            assert!(
                !preferred.detached,
                "Prefer falls back when breaking away is denied"
            );
        }
    }
}

#[test]
fn catalog_programs_exist() {
    let system = system_dir().unwrap();
    assert_eq!(tools::catalog().len(), ToolId::ALL.len());
    for info in tools::catalog() {
        let program = system.join(info.program);
        assert!(program.is_file(), "{} is missing", program.display());
        let request = ToolRequest::new(info.id, info.needs_volume.then_some("C:")).unwrap();
        assert!(request.command_line().starts_with(info.program));
    }
    for tool in tools::windows_tools() {
        assert!(
            system.join(tool.program).is_file(),
            "{} is missing",
            tool.program
        );
    }
}

/// Fails the test when anything is launched.
#[derive(Debug)]
struct PanicLauncher;

impl Launcher for PanicLauncher {
    fn launch(&self, command: &CommandSpec, _output: File) -> optimizer_core::Result<Launched> {
        panic!("nothing may be launched here: {command:?}")
    }
}

fn no_probe() -> optimizer_core::Result<StoreHealth> {
    panic!("planning must not read the component store")
}

#[test]
fn plan_on_this_pc_is_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let log_dir = dir.path().join("tools");
    let env = Environment {
        store_health: no_probe,
        ..Environment::SYSTEM
    };
    let runner = ToolRunner::new(Arc::new(PanicLauncher), env, log_dir.clone());

    let volumes = tools::volumes().unwrap();
    let windows = volumes.first().expect("the Windows volume");
    assert!(
        windows.volume.system && windows.volume.error.is_none(),
        "{windows:?}"
    );
    assert_eq!(
        volumes.len(),
        fixed_volumes().unwrap().len(),
        "every fixed volume is listed"
    );
    let letter = windows.volume.letter.clone();

    let system = system_dir().unwrap();
    for info in tools::catalog() {
        let volume = info.needs_volume.then_some(letter.as_str());
        let plan = runner
            .plan(&ToolRequest::new(info.id, volume).unwrap())
            .unwrap();
        assert_eq!(Path::new(&plan.program), system.join(info.program));
        assert_eq!(plan.volume.as_deref(), volume);
        if !optimizer_core::is_elevated() {
            assert_eq!(
                plan.blocked_reason.as_deref(),
                Some("needs administrator rights")
            );
        }
        if let Some(reason) = &plan.blocked_reason {
            assert!(!reason.is_empty());
        }
        assert!(plan.notes.iter().all(|n| !n.is_empty()));
        println!(
            "{:<15} blocked: {:?}; notes: {:?}",
            info.id, plan.blocked_reason, plan.notes
        );
    }
    let check = runner
        .plan(&ToolRequest::new(ToolId::DiskCheck, Some(&letter)).unwrap())
        .unwrap();
    assert!(
        check.notes.iter().any(|n| n.contains("is in use")),
        "{check:?}"
    );
    assert!(!log_dir.exists(), "planning created the log folder");
    assert!(runner.jobs().is_empty());
    assert!(runner.running().is_none());
}
