//! Services: one TOML file each in `/etc/orchid/services`, started in the order their `needs` give.

use serde::Deserialize;
use std::collections::HashMap;
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
    #[serde(default, rename = "type")]
    kind: Kind,
    #[serde(default)]
    restart: Restart,
}

/// `daemon` keeps running; `oneshot` must finish before whatever needs it starts.
#[derive(Deserialize, Default, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Daemon,
    Oneshot,
}

#[derive(Deserialize, Default, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    #[default]
    No,
    OnFailure,
    Always,
}

#[derive(Clone, Debug)]
pub struct Service {
    pub name: String,
    pub description: String,
    pub exec: Vec<String>,
    pub needs: Vec<String>,
    pub kind: Kind,
    pub restart: Restart,
}

impl Service {
    pub fn parse(name: &str, text: &str) -> Result<Service, String> {
        let raw: Raw = toml::from_str(text).map_err(|e| e.message().to_string())?;
        let s = raw.service;
        if s.exec.is_empty() {
            return Err("exec is empty".into());
        }
        Ok(Service { name: name.into(), description: s.description.unwrap_or_default(), exec: s.exec, needs: s.needs, kind: s.kind, restart: s.restart })
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

/// The order to start them in: what a service needs comes first. Names that do not exist
/// and cycles are returned as problems; the services involved are left out.
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
        for n in &services[i].needs {
            match by_name.get(n.as_str()) {
                Some(&j) => ok &= visit(j, services, by_name, state, out, problems),
                None => {
                    problems.push(format!("{}: needs {n}, which does not exist", services[i].name));
                    ok = false;
                }
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
    // a service is kept only if everything it needs was kept
    let kept: Vec<bool> = {
        let mut k = vec![false; services.len()];
        for &i in &out {
            k[i] = true;
        }
        k
    };
    let ordered = out.into_iter().filter(|&i| services[i].needs.iter().all(|n| by_name.get(n.as_str()).is_some_and(|&j| kept[j]))).map(|i| services[i].clone()).collect();
    (ordered, problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str, needs: &[&str]) -> Service {
        Service { name: name.into(), description: String::new(), exec: vec!["/bin/true".into()], needs: needs.iter().map(|s| s.to_string()).collect(), kind: Kind::Daemon, restart: Restart::No }
    }

    fn names(v: &[Service]) -> Vec<&str> {
        v.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn parses_the_file_format() {
        let s = Service::parse("sshd", "[service]\ndescription = \"ssh\"\nexec = [\"/sbin/sshd\", \"-D\"]\nneeds = [\"network\"]\nrestart = \"on-failure\"\n").unwrap();
        assert_eq!((s.name.as_str(), s.exec.len(), s.kind, s.restart), ("sshd", 2, Kind::Daemon, Restart::OnFailure));
        let o = Service::parse("mount", "[service]\nexec = [\"/bin/mount\", \"-a\"]\ntype = \"oneshot\"\n").unwrap();
        assert_eq!((o.kind, o.restart, o.needs.len()), (Kind::Oneshot, Restart::No, 0));
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
    fn missing_needs_and_cycles_leave_the_service_out() {
        let (o, p) = order(vec![svc("ok", &[]), svc("lost", &["nope"]), svc("x", &["y"]), svc("y", &["x"]), svc("dep", &["lost"])]);
        assert_eq!(names(&o), ["ok"]);
        assert!(p.iter().any(|m| m.contains("nope")) && p.iter().any(|m| m.contains("cycle")), "{p:?}");
    }
}
