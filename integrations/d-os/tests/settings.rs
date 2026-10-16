//! The d-os settings store: layers, import/export, history, crashes, and
//! the service protocol.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;

use nucleus_dos_settings::{Layer, Settings};
use nucleus_kv::{Database, Options};
use nucleus_platform::harness;
use nucleus_platform::sim::{CrashMode, Faults, SimPlatform};

fn db_on(p: &Arc<SimPlatform>) -> nucleus_kv::Result<Database> {
    let opts = Options {
        history: 1000,
        wal_segment_bytes: 4096,
        ..Options::default()
    };
    Database::open_on(Arc::clone(p) as _, Path::new("/settings"), opts)
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn layers_are_separate() {
    let p = Arc::new(SimPlatform::new(1));
    let db = db_on(&p).unwrap();
    let mut s = Settings::new(&db);
    s.set(Layer::Machine, "gui.wallpaper", "blue");
    s.set(Layer::User(1000), "gui.wallpaper", "red");
    s.set(Layer::User(10), "keyboard.layout", "hu");
    assert_eq!(s.get(&Layer::Machine, "gui.wallpaper").unwrap(), None); // staged only
    s.commit().unwrap();
    assert_eq!(
        s.get(&Layer::Machine, "gui.wallpaper").unwrap().as_deref(),
        Some("blue")
    );
    assert_eq!(
        s.get(&Layer::User(1000), "gui.wallpaper")
            .unwrap()
            .as_deref(),
        Some("red")
    );
    // user/10 and user/1000 share a prefix as text, not as layers.
    assert_eq!(
        s.dump(&Layer::User(10)).unwrap(),
        pairs(&[("keyboard.layout", "hu")])
    );
    assert_eq!(
        s.layers().unwrap(),
        vec![Layer::Machine, Layer::User(10), Layer::User(1000)]
    );
    s.unset(Layer::User(10), "keyboard.layout");
    s.commit().unwrap();
    assert_eq!(s.layers().unwrap(), vec![Layer::Machine, Layer::User(1000)]);
}

#[test]
fn import_and_export_round_trip() {
    let p = Arc::new(SimPlatform::new(1));
    let db = db_on(&p).unwrap();
    let mut s = Settings::new(&db);
    let conf = "# d-os config\n\nkeyboard.layout = hu\ngui.shell = desktop with spaces\n  net.dhcp=1\nbroken line\n";
    assert_eq!(s.import(&Layer::Machine, conf), 3);
    s.commit().unwrap();
    let exported = s.export(&Layer::Machine).unwrap();
    assert!(exported.starts_with("# d-os settings"));
    // Importing the export into another layer gives the same settings.
    assert_eq!(s.import(&Layer::User(1), &exported), 3);
    s.commit().unwrap();
    assert_eq!(
        s.dump(&Layer::Machine).unwrap(),
        s.dump(&Layer::User(1)).unwrap()
    );
    assert_eq!(
        s.dump(&Layer::Machine).unwrap(),
        pairs(&[
            ("gui.shell", "desktop with spaces"),
            ("keyboard.layout", "hu"),
            ("net.dhcp", "1")
        ])
    );
}

#[test]
fn yesterdays_settings_can_be_read_back() {
    let p = Arc::new(SimPlatform::new(1));
    let db = db_on(&p).unwrap();
    let mut s = Settings::new(&db);
    s.set(Layer::Machine, "theme", "light");
    s.commit().unwrap();
    let yesterday = s.snapshot().unwrap();
    s.set(Layer::Machine, "theme", "dark");
    s.set(Layer::Machine, "font", "big");
    s.commit().unwrap();
    assert_eq!(
        s.dump_at(&Layer::Machine, yesterday).unwrap(),
        pairs(&[("theme", "light")])
    );
    assert_eq!(
        s.dump(&Layer::Machine).unwrap(),
        pairs(&[("font", "big"), ("theme", "dark")])
    );
}

#[test]
fn a_save_is_all_or_nothing_across_crashes() {
    for mode in [CrashMode::DropUnsynced, CrashMode::Torn] {
        harness::run("settings_save_all_or_nothing", 3, |seed| {
            let old = pairs(&[("a", "1"), ("b", "2"), ("c", "3")]);
            let new = pairs(&[("a", "10"), ("c", "30"), ("d", "40")]);
            let save = |p: &Arc<SimPlatform>| -> nucleus_kv::Result<()> {
                let db = db_on(p)?;
                let mut s = Settings::new(&db);
                for (k, v) in &old {
                    s.set(Layer::Machine, k, v);
                }
                s.commit()?;
                // The panel changes three keys and removes one: one save.
                s.set(Layer::Machine, "a", "10");
                s.set(Layer::Machine, "c", "30");
                s.set(Layer::Machine, "d", "40");
                s.unset(Layer::Machine, "b");
                s.commit()?;
                Ok(())
            };
            let dry = Arc::new(SimPlatform::new(seed));
            save(&dry).unwrap();
            let total = dry.sim_fs().op_count();
            for n in 1..=total {
                let p = Arc::new(SimPlatform::new(seed));
                p.sim_fs().set_faults(Faults {
                    crash_mode: mode,
                    ..Faults::default()
                });
                p.sim_fs().crash_after_ops(n);
                let saved = save(&p).is_ok();
                p.sim_fs().crash();
                let db = db_on(&p).unwrap_or_else(|e| panic!("{mode:?} op {n}: {e}"));
                let got = Settings::new(&db).dump(&Layer::Machine).unwrap();
                assert!(
                    got.is_empty() || got == old || got == new,
                    "{mode:?} op {n}: a mix of old and new settings: {got:?}"
                );
                if saved {
                    assert_eq!(got, new, "{mode:?} op {n}: an acknowledged save was lost");
                }
            }
        });
    }
}

#[test]
fn the_service_protocol() {
    let dir = std::env::temp_dir().join(format!("nucleus-settings-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let conf = dir.with_extension("conf");
    std::fs::write(
        &conf,
        "keyboard.layout = hu\n# comment\ngui.wallpaper = /res/sky.bmp\n",
    )
    .unwrap();
    let export = dir.with_extension("export");
    let script = format!(
        "IMPORT machine {conf}\nCOMMIT\nGET machine keyboard.layout\nSET user/1000 gui.wallpaper red sky\n\
         GET user/1000 gui.wallpaper\nCOMMIT\nGET user/1000 gui.wallpaper\nUNSET machine gui.wallpaper\nCOMMIT\n\
         DUMP machine\nLAYERS\nEXPORT user/1000 {export}\nGET nowhere x\nFROB\n\nQUIT\n",
        conf = conf.display(),
        export = export.display()
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_nucleus-settings"))
        .arg(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("READY nucleus-settings"), "{out}");
    let expect = [
        "OK 2",
        "!OK ", // COMMIT -> OK <ts>
        "OK hu",
        "OK",
        "NONE", // staged, not committed
        "!OK ",
        "OK red sky",
        "OK",
        "!OK ",
        "keyboard.layout = hu",
        "END",
        "machine",
        "user/1000",
        "END",
        "OK",
        "!ERR bad layer",
        "!ERR unknown command",
        "BYE",
    ];
    for (i, want) in expect.iter().enumerate() {
        let got = lines[i + 1];
        match want.strip_prefix('!') {
            Some(prefix) => assert!(
                got.starts_with(prefix),
                "line {}: {got:?}, want {want:?}\n{out}",
                i + 1
            ),
            None => assert_eq!(got, *want, "line {}\n{out}", i + 1),
        }
    }
    let exported = std::fs::read_to_string(&export).unwrap();
    assert!(exported.contains("gui.wallpaper = red sky"), "{exported}");
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_file(&conf).unwrap();
    std::fs::remove_file(&export).unwrap();
}
