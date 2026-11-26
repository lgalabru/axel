//! `axel device link|ls|revoke|code` and the sync between an owner's
//! machines (RFD 0004), through the binary.  Another machine is a directory:
//! a stand-in `ssh` (`$AXEL_SSH`) runs the remote command there with that
//! directory as `$HOME`, so `axel host add` really copies this binary over
//! and every request really goes through `axel host rpc`.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use axel_identity::{Keypair, Keystore, attest};
use axel_inbox::EnvelopeBuilder;
use axel_store::{NewAgent, Store, kind};

/// The stand-in ssh: skips the options, takes the target, and runs the
/// command line in `$FAKE_MACHINES/<target>` as that machine's `$HOME`.
const FAKE_SSH: &str = r#"#!/bin/sh
while [ $# -gt 0 ]; do
  case "$1" in
    -o) shift 2 ;;
    -*) shift ;;
    *) break ;;
  esac
done
target="$1"; shift
home="$FAKE_MACHINES/$target"
mkdir -p "$home"
cd "$home" || exit 1
exec env -u AXEL_HOME HOME="$home" PATH="$home/bin:/usr/bin:/bin" sh -c "$*"
"#;

/// A stand-in harness: records that it ran, then exits.
const FAKE_CLAUDE: &str = "#!/bin/sh\necho ran > \"$HOME/claude.ran\"\n";

struct World {
    root: tempfile::TempDir,
}

impl World {
    fn new() -> World {
        let root = tempfile::tempdir().unwrap();
        let ssh = root.path().join("fake-ssh");
        std::fs::write(&ssh, FAKE_SSH).unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir_all(root.path().join("home").join(".axel")).unwrap();
        World { root }
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn machine(&self, label: &str) -> PathBuf {
        self.root.path().join("machines").join(label)
    }

    /// Run axel as the owner's home machine.
    fn here(&self, args: &[&str]) -> Output {
        self.run(&self.home(), Some(&self.home().join(".axel")), args)
    }

    /// Run axel as the machine `label`, the way its own owner would there.
    fn there(&self, label: &str, args: &[&str]) -> Output {
        self.run(&self.machine(label), None, args)
    }

    fn run(&self, home: &Path, axel_home: Option<&Path>, args: &[&str]) -> Output {
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let mut c = Command::new(env!("CARGO_BIN_EXE_axel"));
        c.env_clear()
            .env("HOME", home)
            .env("AXEL_KEYSTORE", "file")
            .env("AXEL_SSH", self.root.path().join("fake-ssh"))
            .env("FAKE_MACHINES", self.root.path().join("machines"))
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .current_dir(home)
            .args(args);
        if let Some(h) = axel_home {
            c.env("AXEL_HOME", h);
        }
        c.output().unwrap()
    }

    fn ok_here(&self, args: &[&str]) -> String {
        let o = self.here(args);
        assert!(o.status.success(), "axel {args:?}: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8(o.stdout).unwrap()
    }

    fn store_here(&self) -> Store {
        Store::open(&self.home().join(".axel").join("axel.db")).unwrap()
    }

    fn store_there(&self, label: &str) -> Store {
        Store::open(&self.machine(label).join(".axel").join("axel.db")).unwrap()
    }

    fn key_here(&self) -> Keypair {
        Keystore::open_at(self.home().join(".axel").join("secrets.json"), false).unwrap().owner().unwrap()
    }

    fn keystore_there(&self, label: &str) -> Keystore {
        std::fs::create_dir_all(self.machine(label).join(".axel")).unwrap();
        Keystore::open_at(self.machine(label).join(".axel").join("secrets.json"), false).unwrap()
    }
}

/// The linux box was a home of its own (#40): a thread, and an agent.
fn make_linux_a_home(w: &World) -> (Keypair, String) {
    let keystore = w.keystore_there("linux");
    let linux = keystore.owner().unwrap();
    let store = w.store_there("linux");
    let thread = store.create_thread(&linux, "linux work", "work started on the linux box", None).unwrap();
    let scout = Keypair::generate();
    keystore.put_agent(&scout).unwrap();
    store
        .insert_agent(&NewAgent {
            pubkey: scout.public().to_base58(),
            name: "scout".into(),
            auth_tag: Some(attest(&linux, &scout.public(), "").unwrap().to_tag()),
            ..Default::default()
        })
        .unwrap();
    (linux, thread.id)
}

fn link_with_code(w: &World) -> String {
    let owner = w.key_here().public().to_base58();
    let code = w.there("linux", &["device", "code", &owner]);
    assert!(code.status.success(), "{}", String::from_utf8_lossy(&code.stderr));
    let code = String::from_utf8(code.stdout).unwrap().trim().to_string();
    w.ok_here(&["device", "link", "linux", "--route", "confirm", "--code", &code])
}

#[test]
fn linking_a_home_makes_its_threads_and_agents_the_owners_and_syncs_both_ways() {
    let w = World::new();
    w.ok_here(&["agent", "new", "reviewer"]);
    let (linux, linux_thread) = make_linux_a_home(&w);

    // A code that doesn't match links nothing.
    let refused = w.here(&["device", "link", "linux", "--route", "confirm", "--code", "0000-0000"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("doesn't match"), "{}", String::from_utf8_lossy(&refused.stderr));
    assert!(w.store_here().device(&linux.public().to_base58()).unwrap().is_none());

    let linked = link_with_code(&w);
    assert!(linked.contains("linux is linked (confirm)"), "{linked}");

    // Here: linux's key speaks for the owner, its thread is ours, its agent listed.
    let here = w.store_here();
    let owner = w.key_here().public().to_base58();
    assert_eq!(here.principal(&linux.public().to_base58()).unwrap(), owner);
    assert!(here.get_thread(&linux_thread).unwrap().is_some(), "linux's thread came over");
    let scout = here.agent_by_name("scout").unwrap().expect("linux's agent is listed here");
    assert_eq!(scout.device.as_deref(), Some(linux.public().to_base58().as_str()), "an instance on linux");

    // There: the owner's profile and the link arrived.
    let there = w.store_there("linux");
    assert_eq!(there.principal(&linux.public().to_base58()).unwrap(), owner);
    assert!(there.profile_by_name("reviewer").unwrap().is_some());

    // The owner writes on linux's thread from here; linux takes it.
    let note = EnvelopeBuilder::new(kind::NOTE, "from the mac").thread(&linux_thread).sign(&w.key_here()).unwrap();
    here.post(&note).unwrap();
    w.ok_here(&["sync", "linux"]);
    assert!(there.rejected_reason(&note.id).unwrap().is_none() && there.get_event(&note.id).unwrap().is_some());

    let listed = w.ok_here(&["device", "ls"]);
    assert!(listed.lines().any(|l| l.starts_with("linux") && l.contains("linked") && l.contains("confirm")), "{listed}");
}

#[test]
fn a_profile_first_run_on_a_device_gets_its_own_instance_there() {
    let w = World::new();
    w.ok_here(&["agent", "new", "reviewer"]);
    w.keystore_there("linux").owner().unwrap();
    // Over ssh the host key in known_hosts is the proof (the stand-in ssh trusts the target).
    w.ok_here(&["device", "link", "linux"]);

    // On linux, the owner starts the reviewer: linux makes its instance.
    let bin = w.machine("linux").join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("claude"), FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(bin.join("claude"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let started = w.there("linux", &["thread", "new", "review it", "--agent", "reviewer", "--scratch", "--runtime", "pane"]);
    assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
    assert!(w.machine("linux").join("claude.ran").exists(), "the harness ran");

    let there = w.store_there("linux");
    let instances = there.profile_instances(&there.profile_by_name("reviewer").unwrap().unwrap().profile_id).unwrap();
    let linux_instance = instances.iter().find(|a| a.device.is_none()).expect("linux's own instance");

    // Back here, the next sync brings linux's instance and its thread.
    w.ok_here(&["sync", "linux"]);
    let linux_key = w.keystore_there("linux").owner().unwrap().public().to_base58();
    let here = w.store_here();
    let seen = here.agent_on_device("reviewer", &linux_key).unwrap().expect("reviewer@linux is known here");
    assert_eq!(seen.pubkey, linux_instance.pubkey);
    let shown = w.ok_here(&["agent", "show", "reviewer"]);
    assert!(shown.contains("on this machine") && shown.contains(&format!("instance {} on linux", linux_instance.pubkey)), "{shown}");
    let threads = here.list_threads(axel_store::ThreadFilter::default()).unwrap();
    assert!(threads.iter().any(|t| t.title == "review it"), "linux's thread is the owner's: {threads:?}");
}

#[test]
fn a_revoked_device_speaks_for_no_one_from_then_on() {
    let w = World::new();
    let (linux, linux_thread) = make_linux_a_home(&w);
    w.ok_here(&["device", "link", "linux"]);
    let revoked = w.ok_here(&["device", "revoke", "linux"]);
    assert!(revoked.contains("linux is revoked"), "{revoked}");

    // Linux signs something later; it arrives here and is refused.
    let later = EnvelopeBuilder::new(kind::NOTE, "still me?")
        .thread(&linux_thread)
        .created_at(u64::try_from(chrono_now()).unwrap() + 60)
        .sign(&linux)
        .unwrap();
    w.store_there("linux").append(&later).unwrap();
    w.ok_here(&["sync", "linux"]);
    let here = w.store_here();
    assert!(here.get_event(&later.id).unwrap().is_some(), "it was carried");
    assert!(here.rejected_reason(&later.id).unwrap().is_some(), "and refused");
    assert_eq!(here.principal(&linux.public().to_base58()).unwrap(), linux.public().to_base58());
}

#[test]
fn a_host_of_another_owner_and_we_keep_our_agents_apart() {
    let w = World::new();
    w.ok_here(&["agent", "new", "reviewer"]);
    // mini is its owner's home, with an agent of its own it has published.
    let mini_keys = w.keystore_there("mini");
    let mini = mini_keys.owner().unwrap();
    let theirs = Keypair::generate();
    mini_keys.put_agent(&theirs).unwrap();
    let mini_store = w.store_there("mini");
    let agent = mini_store
        .insert_agent(&NewAgent {
            pubkey: theirs.public().to_base58(),
            name: "intruder".into(),
            auth_tag: Some(attest(&mini, &theirs.public(), "").unwrap().to_tag()),
            ..Default::default()
        })
        .unwrap();
    let wire = axel_store::ProfileWire::of(&mini_store.profile(&agent.profile_id).unwrap().unwrap(), &mini.public().to_base58());
    let record = EnvelopeBuilder::new(kind::AGENT_PROFILE, serde_json::to_string(&wire).unwrap()).sign(&mini).unwrap();
    mini_store.post(&record).unwrap();
    // It also claims our key as its device, without our consent.
    let claim = axel_store::DeviceRecord {
        op: axel_store::DeviceOp::Link,
        device: w.key_here().public().to_base58(),
        label: "yours".into(),
        route: Some(axel_store::PairingRoute::Ssh),
        consent: None,
    };
    let claim = EnvelopeBuilder::new(kind::OWNER_DEVICE, serde_json::to_string(&claim).unwrap()).sign(&mini).unwrap();
    mini_store.append(&claim).unwrap();

    w.ok_here(&["host", "add", "mini"]);
    w.ok_here(&["sync", "mini"]);
    assert!(w.store_there("mini").profile_by_name("reviewer").unwrap().is_none(), "a plain host isn't one of our devices");
    let here = w.store_here();
    assert!(here.profile_by_name("intruder").unwrap().is_none(), "nor are its agents ours");
    let me = w.key_here().public().to_base58();
    assert_eq!(here.principal(&me).unwrap(), me, "and its claim on our key went nowhere");
}

fn chrono_now() -> i64 {
    i64::try_from(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()).unwrap()
}
