//! Services: one TOML file each in `/etc/orchid/services`. A service listed in
//! `/etc/orchid/enabled` starts at boot; `needs` pull other services in and fail the service
//! when they fail, `wants` and `after` only order.

use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

#[derive(Deserialize)]
struct Raw {
    service: RawService,
}

#[derive(Deserialize)]
struct RawService {
    description: Option<String>,
    exec: Vec<String>,
    #[serde(default)]
    needs: Vec<String>,
    #[serde(default)]
    wants: Vec<String>,
    #[serde(default)]
    after: Vec<String>,
    #[serde(default, rename = "type")]
    kind: Kind,
    #[serde(default)]
    restart: Restart,
    user: Option<String>,
    group: Option<String>,
    workdir: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    output: Option<Output>,
    timeout: Option<u64>,
    #[serde(default)]
    stop: Vec<String>,
    #[serde(default = "normal")]
    modes: Vec<String>,
}

fn normal() -> Vec<String> {
    vec!["normal".into()]
}

/// `daemon` keeps running in the foreground; `oneshot` must finish before whatever needs it
/// starts; `forking` starts a daemon that leaves, the service is up while its control group has
/// processes.
#[derive(Deserialize, Default, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Daemon,
    Oneshot,
    Forking,
}

#[derive(Deserialize, Default, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    #[default]
    No,
    OnFailure,
    Always,
}

/// Where what the service prints goes: every console, `/var/log/orchid/NAME.log` (rotated), or,
/// for a service that talks to a person, the console itself as its terminal.
#[derive(Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Output {
    Console,
    File,
    Tty,
}

#[derive(Clone, Debug)]
pub struct Service {
    pub name: String,
    pub description: String,
    pub exec: Vec<String>,
    pub needs: Vec<String>,
    pub wants: Vec<String>,
    pub after: Vec<String>,
    pub kind: Kind,
    pub restart: Restart,
    pub user: Option<String>,
    pub group: Option<String>,
    pub workdir: Option<String>,
    pub env: Vec<(String, String)>,
    pub output: Output,
    /// Seconds a stopping service gets before it is killed.
    pub timeout: u64,
    /// Run when the service is stopped, before it is signalled (a oneshot that is done: when it is stopped).
    pub stop: Vec<String>,
    /// The boot modes it belongs to: `normal`, `rescue`.
    pub modes: Vec<String>,
}

impl Service {
    pub fn parse(name: &str, text: &str) -> Result<Service, String> {
        let raw: Raw = toml::from_str(text).map_err(|e| e.message().to_string())?;
        let s = raw.service;
        if s.exec.is_empty() {
            return Err("exec is empty".into());
        }
        let output = s.output.unwrap_or(if s.kind == Kind::Oneshot { Output::Console } else { Output::File });
        Ok(Service {
            name: name.into(),
            description: s.description.unwrap_or_default(),
            exec: s.exec,
            needs: s.needs,
            wants: s.wants,
            after: s.after,
            kind: s.kind,
            restart: s.restart,
            user: s.user,
            group: s.group,
            workdir: s.workdir,
            env: s.env.into_iter().collect(),
            output,
            timeout: s.timeout.unwrap_or(5),
            stop: s.stop,
            modes: s.modes,
        })
    }

    /// Everything this service must start after.
    fn before_me(&self) -> impl Iterator<Item = &String> {
        self.needs.iter().chain(&self.wants).chain(&self.after)
    }
}

/// Every `*.toml` of `dir`; a file that does not parse is reported and left out.
pub fn load(dir: &Path) -> (Vec<Service>, Vec<String>) {
    let (mut found, mut problems) = (Vec::new(), Vec::new());
    let mut files: Vec<_> = std::fs::read_dir(dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    files.sort();
    for f in files.iter().filter(|f| f.extension().is_some_and(|e| e == "toml")) {
        let name = f.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        match std::fs::read_to_string(f).map_err(|e| e.to_string()).and_then(|t| Service::parse(&name, &t)) {
            Ok(s) => found.push(s),
            Err(e) => problems.push(format!("{name}: {e}")),
        }
    }
    (found, problems)
}

/// The names in `dir` (`NAME` or `NAME.toml`, usually links to the service files), or `None` when
/// there is no such directory: then every service starts.
pub fn enabled(dir: &Path) -> Option<HashSet<String>> {
    let d = std::fs::read_dir(dir).ok()?;
    Some(d.flatten().map(|e| e.file_name().to_string_lossy().trim_end_matches(".toml").to_string()).collect())
}

/// The order to start them in: what a service needs, wants or comes after goes first. A missing
/// `needs`, and cycles, are returned as problems; the services involved are left out.
pub fn order(services: Vec<Service>) -> (Vec<Service>, Vec<String>) {
    let by_name: HashMap<&str, usize> = services.iter().enumerate().map(|(i, s)| (s.name.as_str(), i)).collect();
    let mut state = vec![0u8; services.len()]; // 0 new, 1 visiting, 2 done
    let (mut out, mut problems): (Vec<usize>, Vec<String>) = (Vec::new(), Vec::new());

    fn visit(i: usize, services: &[Service], by_name: &HashMap<&str, usize>, state: &mut [u8], out: &mut Vec<usize>, problems: &mut Vec<String>) -> bool {
        match state[i] {
            2 => return true,
            1 => {
                problems.push(format!("{}: needs itself through a cycle", services[i].name));
                return false;
            }
            _ => {}
        }
        state[i] = 1;
        let mut ok = true;
        for n in services[i].before_me() {
            match by_name.get(n.as_str()) {
                Some(&j) => {
                    let fine = visit(j, services, by_name, state, out, problems);
                    ok &= fine || !services[i].needs.contains(n);
                }
                None if services[i].needs.contains(n) => {
                    problems.push(format!("{}: needs {n}, which does not exist", services[i].name));
                    ok = false;
                }
                None => {}
            }
        }
        state[i] = 2;
        if ok {
            out.push(i);
        }
        ok
    }

    for i in 0..services.len() {
        visit(i, &services, &by_name, &mut state, &mut out, &mut problems);
    }
    let mut kept = vec![false; services.len()];
    for &i in &out {
        kept[i] = true;
    }
    let ordered = out.into_iter().filter(|&i| services[i].needs.iter().all(|n| by_name.get(n.as_str()).is_some_and(|&j| kept[j]))).map(|i| services[i].clone()).collect();
    (ordered, problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str, needs: &[&str]) -> Service {
        Service::parse(name, &format!("[service]\nexec = [\"/bin/true\"]\nneeds = {needs:?}\n")).unwrap()
    }

    fn names(v: &[Service]) -> Vec<&str> {
        v.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn parses_the_file_format() {
        let s = Service::parse(
            "sshd",
            "[service]\ndescription = \"ssh\"\nexec = [\"/sbin/sshd\", \"-D\"]\nneeds = [\"network\"]\nwants = [\"log\"]\nafter = [\"clock\"]\nrestart = \"on-failure\"\nuser = \"sshd\"\ntimeout = 20\n[service.env]\nLANG = \"C\"\n",
        )
        .unwrap();
        assert_eq!((s.name.as_str(), s.exec.len(), s.kind, s.restart), ("sshd", 2, Kind::Daemon, Restart::OnFailure));
        assert_eq!((s.wants.len(), s.after.len(), s.user.as_deref(), s.timeout, s.output), (1, 1, Some("sshd"), 20, Output::File));
        assert_eq!(s.env, vec![("LANG".to_string(), "C".to_string())]);
        let o = Service::parse("mount", "[service]\nexec = [\"/bin/mount\", \"-a\"]\ntype = \"oneshot\"\n").unwrap();
        assert_eq!((o.kind, o.restart, o.needs.len(), o.output, o.timeout), (Kind::Oneshot, Restart::No, 0, Output::Console, 5));
        assert!(Service::parse("x", "[service]\nexec = []\n").is_err());
        assert!(Service::parse("x", "[service]\n").is_err());
    }

    #[test]
    fn needs_come_first() {
        let (o, p) = order(vec![svc("c", &["b"]), svc("b", &["a"]), svc("a", &[])]);
        assert_eq!(names(&o), ["a", "b", "c"]);
        assert!(p.is_empty());
    }

    #[test]
    fn wants_and_after_only_order() {
        let mut b = svc("b", &[]);
        b.wants = vec!["a".into(), "ghost".into()];
        b.after = vec!["c".into()];
        let (o, p) = order(vec![b, svc("c", &[]), svc("a", &[])]);
        assert_eq!(names(&o), ["a", "c", "b"]);
        assert!(p.is_empty(), "{p:?}");
    }

    #[test]
    fn missing_needs_and_cycles_leave_the_service_out() {
        let (o, p) = order(vec![svc("ok", &[]), svc("lost", &["nope"]), svc("x", &["y"]), svc("y", &["x"]), svc("dep", &["lost"])]);
        assert_eq!(names(&o), ["ok"]);
        assert!(p.iter().any(|m| m.contains("nope")) && p.iter().any(|m| m.contains("cycle")), "{p:?}");
    }

    #[test]
    fn enabled_lists_names_without_the_suffix() {
        let d = tempfile::tempdir().unwrap();
        assert!(enabled(&d.path().join("none")).is_none());
        std::fs::write(d.path().join("a.toml"), "").unwrap();
        std::fs::write(d.path().join("b"), "").unwrap();
        let e = enabled(d.path()).unwrap();
        assert!(e.contains("a") && e.contains("b") && e.len() == 2);
    }
}
