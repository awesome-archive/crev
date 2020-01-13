use crate::{opts, opts::CrateSelector};
use cargo::{
    core::{
        dependency::{Dependency, Kind},
        manifest::ManifestMetadata,
        package::PackageSet,
        registry::PackageRegistry,
        resolver::Method,
        source::SourceMap,
        InternedString, Package, PackageId, PackageIdSpec, Resolve, SourceId, Workspace,
    },
    ops,
    util::{self, important_paths::find_root_manifest_for_wd, CargoResult, Cfg, Rustc},
};
use crev_common::convert::OptionDeref;
use crev_lib;
use failure::format_err;
use petgraph::graph::NodeIndex;
use std::{
    collections::{hash_map::Entry, BTreeSet, HashMap, HashSet},
    env,
    path::PathBuf,
    rc::Rc,
    str::{self, FromStr},
};

use crate::{crates_io, prelude::*};

#[derive(Debug)]
struct Node {
    id: PackageId,
    #[allow(unused)]
    metadata: ManifestMetadata,
}

#[derive(Debug)]
pub struct Graph {
    graph: petgraph::Graph<Node, Kind>,
    nodes: HashMap<PackageId, NodeIndex>,
}

impl Graph {
    pub fn get_all_pkg_ids<'s>(&'s self) -> impl Iterator<Item = PackageId> + 's {
        self.nodes.keys().cloned()
    }

    pub fn get_dependencies_of<'s>(
        &'s self,
        pkg_id: PackageId,
    ) -> impl Iterator<Item = PackageId> + 's {
        self.nodes
            .get(&pkg_id)
            .into_iter()
            .flat_map(move |node_idx| {
                self.graph
                    .neighbors_directed(*node_idx, petgraph::Direction::Outgoing)
            })
            .map(move |node_idx| self.graph.node_weight(node_idx).unwrap().id)
    }

    pub fn get_reverse_dependencies_of<'s>(
        &'s self,
        pkg_id: PackageId,
    ) -> impl Iterator<Item = PackageId> + 's {
        self.nodes
            .get(&pkg_id)
            .into_iter()
            .flat_map(move |node_idx| {
                self.graph
                    .neighbors_directed(*node_idx, petgraph::Direction::Incoming)
            })
            .map(move |node_idx| self.graph.node_weight(node_idx).unwrap().id)
    }

    pub fn get_recursive_dependencies_of(&self, root_pkg_id: PackageId) -> HashSet<PackageId> {
        let mut pending = BTreeSet::new();
        let mut processed = HashSet::new();

        pending.insert(root_pkg_id);

        while let Some(pkg_id) = pending.iter().next().cloned() {
            pending.remove(&pkg_id);

            if processed.contains(&pkg_id) {
                continue;
            } else {
                processed.insert(pkg_id);
            }

            if let Some(node_idx) = self.nodes.get(&pkg_id) {
                for node_idx in self
                    .graph
                    .neighbors_directed(*node_idx, petgraph::Direction::Outgoing)
                {
                    pending.insert(self.graph.node_weight(node_idx).unwrap().id);
                }
            } else {
                eprintln!(
                    "No node for {} when checking recdeps for {}",
                    pkg_id, root_pkg_id
                );
            }
        }

        processed.remove(&root_pkg_id);

        processed
    }
}

fn get_cfgs(rustc: &Rustc, target: Option<&str>) -> CargoResult<Option<Vec<Cfg>>> {
    let mut process = util::process(&rustc.path);
    process.arg("--print=cfg").env_remove("RUST_LOG");
    if let Some(ref s) = target {
        process.arg("--target").arg(s);
    }

    let output = match process.exec_with_output() {
        Ok(output) => output,
        Err(e) => return Err(e),
    };
    let output = str::from_utf8(&output.stdout).unwrap();
    let lines = output.lines();
    Ok(Some(
        lines.map(Cfg::from_str).collect::<CargoResult<Vec<_>>>()?,
    ))
}

fn our_resolve<'a, 'cfg>(
    registry: &mut PackageRegistry<'cfg>,
    workspace: &'a Workspace<'cfg>,
    features: &[String],
    all_features: bool,
    no_default_features: bool,
    no_dev_dependencies: bool,
) -> CargoResult<(PackageSet<'cfg>, Resolve)> {
    // there is bunch of slightly different ways to do it,
    // so I leave some dead code around, in case I want to
    // try the other ones, in some near future

    // this one will create a `Cargo.lock` file if it didn't exist before
    // good? not good? it also uses the registry to make it possible
    // the other methods
    let (packages, resolve) = ops::resolve_ws(workspace)?;

    let method = Method::Required {
        dev_deps: !no_dev_dependencies,
        features: Rc::new(features.iter().map(|s| InternedString::new(s)).collect()),
        all_features,
        uses_default_features: !no_default_features,
    };

    let specs: Vec<_> = workspace
        .members()
        .map(|m| m.summary().package_id())
        .map(PackageIdSpec::from_package_id)
        .collect();

    let resolve = ops::resolve_with_previous(
        registry,
        workspace,
        method,
        Some(&resolve),
        None,
        &specs,
        true,
    )?;

    Ok((packages, resolve))

    /*
    // this method does not allow passing no_dev_dependencies
    let specs: Vec<_> = roots
        .map(|id| PackageIdSpec::from_package_id(id))
        .collect();

    ops::resolve_ws_precisely(
        workspace,
        features,
        all_features,
        no_default_features,
        &specs,
    )
    */

    /*
    // this does not update/create `Cargo.lock` AFAIU
    let method = Method::Required {
        dev_deps: !no_dev_dependencies,
        features: Rc::new(features.iter().map(|s| InternedString::new(s)).collect()),
        all_features,
        uses_default_features: !no_default_features,
    };

    let specs: Vec<_> = roots
        .map(|id| PackageIdSpec::from_package_id(id))
        .collect();

    ops::resolve_ws_with_method(workspace, method, &specs)
    */
}

fn build_graph<'a>(
    resolve: &'a Resolve,
    packages: &'a PackageSet<'_>,
    roots: impl Iterator<Item = PackageId>,
    target: Option<&str>,
    cfgs: Option<&[Cfg]>,
) -> CargoResult<Graph> {
    let mut graph = Graph {
        graph: petgraph::Graph::new(),
        nodes: HashMap::new(),
    };

    let mut pending = vec![];
    for root in roots {
        let node = Node {
            id: root,
            metadata: packages.get_one(root)?.manifest().metadata().clone(),
        };
        graph.nodes.insert(root, graph.graph.add_node(node));
        pending.push(root);
    }

    while let Some(pkg_id) = pending.pop() {
        let idx = graph.nodes[&pkg_id];
        let pkg = packages.get_one(pkg_id)?;

        for raw_dep_id in resolve.deps_not_replaced(pkg_id) {
            let it = pkg
                .dependencies()
                .iter()
                .filter(|d| d.matches_ignoring_source(raw_dep_id))
                .filter(|d| {
                    d.platform()
                        .and_then(|p| target.map(|t| p.matches(t, cfgs)))
                        .unwrap_or(true)
                });

            let dep_id = match resolve.replacement(raw_dep_id) {
                Some(id) => id,
                None => raw_dep_id,
            };
            for dep in it {
                let dep_idx = match graph.nodes.entry(dep_id) {
                    Entry::Occupied(e) => *e.get(),
                    Entry::Vacant(e) => {
                        pending.push(dep_id);
                        let node = Node {
                            id: dep_id,
                            metadata: packages.get_one(dep_id)?.manifest().metadata().clone(),
                        };
                        *e.insert(graph.graph.add_node(node))
                    }
                };
                graph.graph.add_edge(idx, dep_idx, dep.kind());
            }
        }
    }

    Ok(graph)
}

/// A handle to the current Rust project
pub struct Repo {
    manifest_path: PathBuf,
    config: cargo::util::config::Config,
    cargo_opts: opts::CargoOpts,
    #[allow(unused)]
    features_set: BTreeSet<InternedString>,
    features_list: Vec<String>,
}

impl Repo {
    pub fn auto_open_cwd_default() -> Result<Self> {
        Self::auto_open_cwd(Default::default())
    }

    pub fn auto_open_cwd(cargo_opts: opts::CargoOpts) -> Result<Self> {
        cargo::core::enable_nightly_features();
        let manifest_path = if let Some(ref path) = cargo_opts.manifest_path {
            path.to_owned()
        } else {
            let cwd = env::current_dir()?;
            find_root_manifest_for_wd(&cwd)?
        };
        let mut config = cargo::util::config::Config::default()?;
        config.configure(
            0,
            None,
            &None,
            /* frozen: */ false,
            /* locked: */ true,
            /* offline: */ false,
            &None,
            &cargo_opts.unstable_flags,
        )?;
        let features_set =
            Method::split_features(&[cargo_opts.features.clone().unwrap_or_else(String::new)]);

        let features_list = features_set.iter().map(|i| i.as_str().to_owned()).collect();
        Ok(Repo {
            manifest_path,
            config,
            features_set,
            features_list,
            cargo_opts,
        })
    }

    pub fn name(&self) -> std::borrow::Cow<'_, str> {
        self.manifest_path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
    }

    fn workspace(&self) -> CargoResult<Workspace<'_>> {
        Workspace::new(&self.manifest_path, &self.config)
    }

    // TODO: Do we even need it? We should just always use a default/empty
    // registry or something? We don't have anything custom to add.
    fn registry<'a>(
        &'a self,
        source_ids: impl Iterator<Item = SourceId>,
    ) -> CargoResult<PackageRegistry<'a>> {
        let _lock = self.config.acquire_package_cache_lock()?;
        let mut registry = PackageRegistry::new(&self.config)?;
        registry.add_sources(source_ids)?;
        Ok(registry)
    }

    pub fn get_dependency_graph(&self, roots: Vec<PackageId>) -> CargoResult<Graph> {
        let workspace = self.workspace()?;

        let mut registry = self.registry(
            workspace
                .members()
                .map(|m| m.summary().source_id().to_owned()),
        )?;
        // let root_sources: Vec<_> = roots.iter().map(|p| p.source_id().to_owned()).collect();
        // let mut registry = self.registry(root_sources.into_iter())?;

        let (packages, resolve) = our_resolve(
            &mut registry,
            &workspace,
            &self.features_list,
            self.cargo_opts.all_features,
            self.cargo_opts.no_default_features,
            self.cargo_opts.no_dev_dependencies,
        )?;

        let rustc = self.config.load_global_rustc(Some(&workspace))?;

        let target = if let Some(ref target) = self.cargo_opts.target {
            Some(target.as_ref().unwrap_or(&rustc.host).as_str())
        } else {
            None
        };

        let cfgs = get_cfgs(&rustc, target)?;
        let graph = build_graph(
            &resolve,
            &packages,
            roots.into_iter(),
            target,
            cfgs.as_ref().map(|r| &**r),
        )?;

        Ok(graph)
    }

    pub fn update_source(&self) -> Result<()> {
        let mut source = self.load_source()?;
        let _lock = self.config.acquire_package_cache_lock()?;
        source.update()?;
        Ok(())
    }

    pub fn update_counts(&self) -> Result<()> {
        let local = crev_lib::Local::auto_create_or_open()?;
        let crates_io = crates_io::Client::new(&local)?;

        self.for_every_non_local_dep_crate(|crate_| {
            let _ = crates_io.get_downloads_count(&crate_.name(), &crate_.version());
            Ok(())
        })?;

        Ok(())
    }

    pub fn load_source<'a>(&'a self) -> Result<Box<dyn cargo::core::source::Source + 'a>> {
        let source_id = SourceId::crates_io(&self.config)?;
        let map = cargo::sources::SourceConfigMap::new(&self.config)?;
        let yanked_whitelist = HashSet::new();
        let source = map.load(source_id, &yanked_whitelist)?;
        Ok(source)
    }

    pub fn load_source_with_whitelist<'a>(
        &'a self,
        yanked_whitelist: HashSet<PackageId>,
    ) -> Result<Box<dyn cargo::core::source::Source + 'a>> {
        let source_id = SourceId::crates_io(&self.config)?;
        let map = cargo::sources::SourceConfigMap::new(&self.config)?;
        let source = map.load(source_id, &yanked_whitelist)?;
        Ok(source)
    }

    /// Run `f` for every non-local dependency crate
    ///
    /// TODO: This function doing downloads etc. is meh.
    /// Get rid of it.
    pub fn for_every_non_local_dep_crate(
        &self,
        mut f: impl FnMut(&Package) -> Result<()>,
    ) -> Result<()> {
        let workspace = self.workspace()?;

        // TODO: all pkgs instead
        let roots: Vec<_> = workspace
            .members()
            .map(|m| m.summary().package_id())
            .collect();

        let mut registry = self.registry(roots.iter().map(|pkgid| pkgid.source_id()))?;

        let (package_set, _resolve) = our_resolve(
            &mut registry,
            &workspace,
            &self.features_list,
            self.cargo_opts.all_features,
            self.cargo_opts.no_default_features,
            self.cargo_opts.no_dev_dependencies,
        )?;
        let mut source = self.load_source()?;

        let pkgs = package_set.get_many(package_set.package_ids())?;

        for pkg in pkgs {
            if !pkg.summary().source_id().is_registry() {
                continue;
            }
            if !pkg.root().exists() {
                source.download(pkg.package_id())?;
            }

            f(&pkg)?;
        }

        Ok(())
    }

    /// Run `f` for every non-local dependency crate
    pub fn for_every_non_local_dep_crate_id(
        &self,
        mut f: impl FnMut(&PackageId) -> Result<()>,
    ) -> Result<()> {
        let workspace = self.workspace()?;

        // TODO: all pkgs instead
        let roots: Vec<_> = workspace
            .members()
            .map(|m| m.summary().package_id())
            .collect();

        let mut registry = self.registry(roots.iter().map(|pkgid| pkgid.source_id()))?;

        let (package_set, _resolve) = our_resolve(
            &mut registry,
            &workspace,
            &self.features_list,
            self.cargo_opts.all_features,
            self.cargo_opts.no_default_features,
            self.cargo_opts.no_dev_dependencies,
        )?;

        for pkg_id in package_set.package_ids() {
            if !pkg_id.source_id().is_registry() {
                continue;
            }

            f(&pkg_id)?;
        }

        Ok(())
    }

    /*
    pub fn get_deps_package_set(&self) -> Result<PackageSet<'_>> {
        let workspace = self.workspace()?;
        let specs = cargo::ops::Packages::All.to_package_id_specs(&workspace)?;
        let (package_set, _resolve) = cargo::ops::resolve_ws_precisely(
            &workspace,
            &self.features_list,
            self.cargo_opts.all_features,
            self.cargo_opts.no_default_features,
            &specs,
        )?;
        Ok(package_set)
    }
    */

    pub fn get_package_set<'a>(&'a self) -> Result<(PackageSet<'a>, Resolve)> {
        let workspace = self.workspace()?;

        let mut registry = self.registry(vec![].into_iter())?;

        Ok(our_resolve(
            &mut registry,
            &workspace,
            &self.features_list,
            self.cargo_opts.all_features,
            self.cargo_opts.no_default_features,
            self.cargo_opts.no_dev_dependencies,
        )?)
    }

    pub fn find_dependency_pkg_id_by_selector(
        &self,
        name: &str,
        version: Option<&Version>,
    ) -> Result<Option<PackageId>> {
        let mut ret = vec![];

        self.for_every_non_local_dep_crate_id(|pkg_id| {
            if name == pkg_id.name().as_str()
                && (version.is_none() || version == Some(&pkg_id.version()))
            {
                ret.push(pkg_id.to_owned());
            }
            Ok(())
        })?;

        match ret.len() {
            0 => Ok(None),
            1 => Ok(Some(ret[0])),
            n => bail!(
                "Ambiguous selection: {} matches found: {}",
                n,
                ret.iter()
                    .map(|pkgid| pkgid.version().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    pub fn get_crate(&self, pkg_id: &PackageId) -> Result<Package> {
        // We need to whitelist the crate, in case it was yanked
        let mut yanked_whitelist = HashSet::default();
        yanked_whitelist.insert(pkg_id.to_owned());
        let source = self.load_source_with_whitelist(yanked_whitelist)?;

        let mut source_map = SourceMap::new();
        source_map.insert(source);
        let package_set =
            cargo::core::PackageSet::new(&[pkg_id.to_owned()], source_map, &self.config)?;
        Ok(package_set.get_one(pkg_id.to_owned())?.to_owned())
    }

    pub fn find_independent_pkg_id_by_selector(
        &self,
        name: &str,
        version: Option<&Version>,
    ) -> Result<Option<PackageId>> {
        let mut source = if let Some(version) = version {
            // special case - we need to whitelist the crate, in case it was yanked
            let mut yanked_whitelist = HashSet::default();
            let source_id = SourceId::crates_io(&self.config)?;
            yanked_whitelist.insert(PackageId::new(name, version, source_id)?);
            self.load_source_with_whitelist(yanked_whitelist)?
        } else {
            self.load_source()?
        };
        let mut summaries = vec![];
        let version_str = version.map(ToString::to_string);
        let dependency_request = Dependency::parse_no_deprecated(
            name,
            OptionDeref::as_deref(&version_str),
            source.source_id(),
        )?;
        let _lock = self.config.acquire_package_cache_lock()?;
        source.query(&dependency_request, &mut |summary| {
            summaries.push(summary.clone())
        })?;
        let summary = if let Some(version) = version {
            summaries.iter().find(|s| s.version() == version)
        } else {
            summaries.iter().max_by_key(|s| s.version())
        };

        Ok(summary.map(|s| s.package_id()))
    }

    pub fn find_pkgid(
        &self,
        name: &str,
        version: Option<&Version>,
        unrelated: bool,
    ) -> Result<PackageId> {
        if unrelated {
            Ok(
                    self.find_independent_pkg_id_by_selector(name, version)?
                        .ok_or_else(|| format_err!("Could not find requested crate. Try updating cargo's registry index cache?"))?
                )
        } else {
            Ok(self.find_dependency_pkg_id_by_selector(&name, version)?
                    .ok_or_else(|| format_err!("Could not find requested crate. Try `-u` if the crate is not a dependency."))?
                    )
        }
    }

    pub fn find_pkgid_by_crate_selector(&self, sel: &CrateSelector) -> Result<PackageId> {
        sel.ensure_name_given()?;

        let version = sel.version.clone().map(Version::from);

        self.find_pkgid(sel.name.as_ref().unwrap(), version.as_ref(), sel.unrelated)
    }

    pub fn find_roots_by_crate_selector(&self, sel: &CrateSelector) -> Result<Vec<PackageId>> {
        if let Some(_name) = &sel.name {
            self.find_pkgid_by_crate_selector(sel).map(|i| vec![i])
        } else {
            Ok(self
                .workspace()?
                .members()
                .map(|m| m.package_id())
                .collect())
        }
    }
}
