//! Which cgroup hierarchies the machine has, and which of them serves each feature. Kernels differ (v2 only, v1 only,
//! or the hybrid of both with v2 holding no controllers), so the controller never assumes one: it asks the layout, and a
//! feature no hierarchy can serve is reported as unbound with the reason, never silently skipped.

use crate::fs::Host;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Api {
    V1,
    V2,
}

impl Api {
    pub fn name(self) -> &'static str {
        match self {
            Api::V1 => "v1",
            Api::V2 => "v2",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Feature {
    Memory,
    Pids,
    CpuQuota,
    CpuWeight,
    Cpuset,
    Io,
    Freeze,
    Kill,
    Pressure,
}

impl Feature {
    pub const ALL: [Feature; 9] = [
        Feature::Memory,
        Feature::Pids,
        Feature::CpuQuota,
        Feature::CpuWeight,
        Feature::Cpuset,
        Feature::Io,
        Feature::Freeze,
        Feature::Kill,
        Feature::Pressure,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Feature::Memory => "memory",
            Feature::Pids => "pids",
            Feature::CpuQuota => "cpu.quota",
            Feature::CpuWeight => "cpu.weight",
            Feature::Cpuset => "cpuset",
            Feature::Io => "io",
            Feature::Freeze => "freeze",
            Feature::Kill => "kill",
            Feature::Pressure => "pressure",
        }
    }

    /// The controller that serves the feature, as named by (v2, v1); `None` for what the hierarchy itself provides.
    fn controller(self) -> Option<(&'static str, &'static str)> {
        match self {
            Feature::Memory => Some(("memory", "memory")),
            Feature::Pids => Some(("pids", "pids")),
            Feature::CpuQuota | Feature::CpuWeight => Some(("cpu", "cpu")),
            Feature::Cpuset => Some(("cpuset", "cpuset")),
            Feature::Io => Some(("io", "blkio")),
            Feature::Freeze | Feature::Kill | Feature::Pressure => None,
        }
    }
}

/// Which interface freezes and kills: `Auto` prefers v2 (`cgroup.freeze`, `cgroup.kill`) over the v1 freezer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    Auto,
    V1,
    V2,
}

impl Lifecycle {
    pub fn parse(s: &str) -> Result<Lifecycle, String> {
        match s {
            "auto" => Ok(Lifecycle::Auto),
            "v1" => Ok(Lifecycle::V1),
            "v2" => Ok(Lifecycle::V2),
            _ => Err(format!("`{s}` is not a lifecycle (auto v1 v2)")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    V2,
    Hybrid,
    V1,
    None,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::V2 => "v2",
            Mode::Hybrid => "hybrid",
            Mode::V1 => "v1",
            Mode::None => "none",
        }
    }
}

/// One mounted hierarchy and the controllers it carries.
#[derive(Clone, Debug)]
pub struct Hier {
    pub api: Api,
    pub root: String,
    pub controllers: BTreeSet<String>,
}

const V1_CONTROLLERS: [&str; 7] = [
    "memory", "pids", "cpu", "cpuacct", "cpuset", "blkio", "freezer",
];

#[derive(Debug)]
pub struct Layout {
    pub mode: Mode,
    pub hiers: Vec<Hier>,
    /// Feature -> index of the hierarchy that serves it.
    bound: BTreeMap<Feature, usize>,
    /// Feature -> why no hierarchy serves it.
    unbound: BTreeMap<Feature, String>,
}

impl Layout {
    pub fn detect(host: &dyn Host, lifecycle: Lifecycle) -> Layout {
        let mounts = host.read("/proc/mounts").unwrap_or_default();
        let mut hiers: Vec<Hier> = Vec::new();
        for line in mounts.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 4 || hiers.iter().any(|h| h.root == f[1]) {
                continue;
            }
            let (root, kind, opts) = (f[1], f[2], f[3]);
            match kind {
                "cgroup2" => {
                    let list = host
                        .read(&format!("{root}/cgroup.controllers"))
                        .unwrap_or_default();
                    let controllers = list.split_whitespace().map(str::to_string).collect();
                    hiers.push(Hier {
                        api: Api::V2,
                        root: root.to_string(),
                        controllers,
                    });
                }
                "cgroup" => {
                    let controllers: BTreeSet<String> = opts
                        .split(',')
                        .filter(|o| V1_CONTROLLERS.contains(o))
                        .map(str::to_string)
                        .collect();
                    // a named hierarchy without controllers (name=systemd) serves nothing here
                    if !controllers.is_empty() {
                        hiers.push(Hier {
                            api: Api::V1,
                            root: root.to_string(),
                            controllers,
                        });
                    }
                }
                _ => {}
            }
        }
        let has = |api: Api| hiers.iter().any(|h| h.api == api);
        let mode = match (has(Api::V2), has(Api::V1)) {
            (true, true) => Mode::Hybrid,
            (true, false) => Mode::V2,
            (false, true) => Mode::V1,
            (false, false) => Mode::None,
        };
        let mut bound = BTreeMap::new();
        let mut unbound = BTreeMap::new();
        for f in Feature::ALL {
            let pick = |api: Api, ctl: &str| {
                hiers
                    .iter()
                    .position(|h| h.api == api && h.controllers.contains(ctl))
            };
            let found = match f.controller() {
                Some((c2, c1)) => pick(Api::V2, c2)
                    .or_else(|| pick(Api::V1, c1))
                    .ok_or(format!("no hierarchy carries the {c2} controller")),
                None => {
                    let v2 = hiers.iter().position(|h| h.api == Api::V2);
                    let v1 = pick(Api::V1, "freezer");
                    match (f, lifecycle) {
                        (Feature::Pressure, Lifecycle::V1) => {
                            Err("pressure needs cgroup v2 (lifecycle v1 was asked for)".to_string())
                        }
                        (Feature::Pressure, _) => v2.ok_or("pressure needs cgroup v2".to_string()),
                        (_, Lifecycle::Auto) => v2
                            .or(v1)
                            .ok_or("neither cgroup v2 nor the v1 freezer is mounted".to_string()),
                        (_, Lifecycle::V2) => v2.ok_or("cgroup v2 is not mounted".to_string()),
                        (_, Lifecycle::V1) => v1.ok_or("the v1 freezer is not mounted".to_string()),
                    }
                }
            };
            match found {
                Ok(i) => {
                    bound.insert(f, i);
                }
                Err(why) => {
                    unbound.insert(f, why);
                }
            }
        }
        Layout {
            mode,
            hiers,
            bound,
            unbound,
        }
    }

    pub fn is_bound(&self, f: Feature) -> bool {
        self.bound.contains_key(&f)
    }

    pub fn why(&self, f: Feature) -> String {
        self.unbound
            .get(&f)
            .cloned()
            .unwrap_or_else(|| "bound".into())
    }

    pub fn index(&self, f: Feature) -> Option<usize> {
        self.bound.get(&f).copied()
    }

    pub fn api(&self, f: Feature) -> Option<Api> {
        self.index(f).map(|i| self.hiers[i].api)
    }

    /// The hierarchies that carry the domain tree: every one that serves a bound feature.
    pub fn members(&self) -> Vec<usize> {
        self.bound
            .values()
            .copied()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// `layout=`, `hierarchy.N=` and `feature.NAME=` lines for `resctl probe`.
    pub fn describe(&self) -> Vec<String> {
        let mut out = vec![format!("layout={}", self.mode.name())];
        for (i, h) in self.hiers.iter().enumerate() {
            let list: Vec<&str> = h.controllers.iter().map(String::as_str).collect();
            out.push(format!(
                "hierarchy.{i}={}:{} controllers={}",
                h.api.name(),
                h.root,
                list.join(",")
            ));
        }
        for f in Feature::ALL {
            let how = match self.index(f) {
                Some(i) => format!("{}:{}", self.hiers[i].api.name(), self.hiers[i].root),
                None => format!("unbound:{}", self.why(f)),
            };
            out.push(format!("feature.{}={how}", f.name()));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::fake::FakeHost;

    const ALL_V2: [&str; 6] = ["cpuset", "cpu", "io", "memory", "pids", "hugetlb"];

    fn hybrid() -> FakeHost {
        let h = FakeHost::new();
        h.add_v1("/c/systemd", &[]);
        for c in [
            "memory", "pids", "cpu", "cpuacct", "cpuset", "blkio", "freezer",
        ] {
            h.add_v1(&format!("/c/{c}"), &[c]);
        }
        h.add_v2("/c/unified", &["hugetlb"]);
        h
    }

    #[test]
    fn a_v2_machine_binds_everything_to_the_one_hierarchy() {
        let h = FakeHost::new();
        h.add_v2("/c", &ALL_V2);
        let l = Layout::detect(&h, Lifecycle::Auto);
        assert_eq!(l.mode, Mode::V2);
        assert_eq!(l.members(), vec![0]);
        for f in Feature::ALL {
            assert_eq!(l.api(f), Some(Api::V2), "{}", f.name());
        }
    }

    #[test]
    fn a_hybrid_machine_takes_limits_from_v1_and_the_lifecycle_from_v2() {
        let l = Layout::detect(&hybrid(), Lifecycle::Auto);
        assert_eq!(l.mode, Mode::Hybrid);
        for f in [
            Feature::Memory,
            Feature::Pids,
            Feature::CpuQuota,
            Feature::CpuWeight,
            Feature::Cpuset,
            Feature::Io,
        ] {
            assert_eq!(l.api(f), Some(Api::V1), "{}", f.name());
        }
        for f in [Feature::Freeze, Feature::Kill, Feature::Pressure] {
            assert_eq!(l.api(f), Some(Api::V2), "{}", f.name());
        }
        assert_eq!(l.hiers[l.index(Feature::Io).unwrap()].root, "/c/blkio");
        // the named systemd hierarchy and the unused cpuacct serve nothing; the v2 one serves the lifecycle
        let roots: Vec<&str> = l
            .members()
            .iter()
            .map(|&i| l.hiers[i].root.as_str())
            .collect();
        assert!(
            roots.contains(&"/c/unified")
                && !roots.contains(&"/c/systemd")
                && !roots.contains(&"/c/cpuacct")
        );
        assert!(!roots.contains(&"/c/freezer"));
    }

    #[test]
    fn the_lifecycle_can_be_forced_to_the_v1_freezer() {
        let l = Layout::detect(&hybrid(), Lifecycle::V1);
        assert_eq!(l.api(Feature::Freeze), Some(Api::V1));
        assert_eq!(l.api(Feature::Kill), Some(Api::V1));
        assert!(
            !l.is_bound(Feature::Pressure) && l.why(Feature::Pressure).contains("lifecycle v1")
        );
        assert!(!l.members().iter().any(|&i| l.hiers[i].api == Api::V2));
        let l = Layout::detect(&hybrid(), Lifecycle::V2);
        assert_eq!(l.api(Feature::Kill), Some(Api::V2));
    }

    #[test]
    fn a_forced_lifecycle_the_machine_lacks_is_unbound_with_the_reason() {
        let h = FakeHost::new();
        h.add_v2("/c", &ALL_V2);
        let l = Layout::detect(&h, Lifecycle::V1);
        assert!(!l.is_bound(Feature::Kill) && l.why(Feature::Kill).contains("v1 freezer"));
        let h = FakeHost::new();
        h.add_v1("/c/memory", &["memory"]);
        let l = Layout::detect(&h, Lifecycle::Auto);
        assert_eq!(l.mode, Mode::V1);
        assert!(
            l.is_bound(Feature::Memory) && !l.is_bound(Feature::Pids) && !l.is_bound(Feature::Kill)
        );
        assert!(l.why(Feature::Pids).contains("pids"));
    }

    #[test]
    fn no_cgroup_filesystem_is_layout_none() {
        let l = Layout::detect(&FakeHost::new(), Lifecycle::Auto);
        assert_eq!(l.mode, Mode::None);
        assert!(l.members().is_empty());
        assert!(Feature::ALL.iter().all(|&f| !l.is_bound(f)));
        assert_eq!(l.describe()[0], "layout=none");
    }

    #[test]
    fn describe_names_every_hierarchy_and_feature() {
        let lines = Layout::detect(&hybrid(), Lifecycle::Auto).describe();
        assert_eq!(lines[0], "layout=hybrid");
        assert!(lines.iter().any(|l| l == "feature.memory=v1:/c/memory"));
        assert!(lines.iter().any(|l| l == "feature.kill=v2:/c/unified"));
        assert!(lines.iter().any(
            |l| l.starts_with("hierarchy.") && l.contains("v2:/c/unified controllers=hugetlb")
        ));
        assert_eq!(
            lines.iter().filter(|l| l.starts_with("feature.")).count(),
            Feature::ALL.len()
        );
    }
}
