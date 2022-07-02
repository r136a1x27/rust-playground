#![deny(rust_2018_idioms)]

use cargo::{
    core::{
        compiler::{CompileKind, CompileTarget, TargetInfo},
        package::PackageSet,
        registry::PackageRegistry,
        resolver::{self, features::RequestedFeatures, ResolveOpts, VersionPreferences},
        source::SourceMap,
        Dependency, Package, PackageId, Source, SourceId, Summary, TargetKind,
    },
    sources::RegistrySource,
    util::{interning::InternedString, Config, VersionExt},
};
use itertools::Itertools;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    io::Read,
    task::Poll,
};

const PLAYGROUND_TARGET_PLATFORM: &str = "x86_64-unknown-linux-gnu";

struct GlobalState<'cfg> {
    config: &'cfg Config,
    target_info: TargetInfo,
    registry: PackageRegistry<'cfg>,
    crates_io: SourceId,
    source: RegistrySource<'cfg>,
    modifications: &'cfg Modifications,
}

/// The list of crates from crates.io
#[derive(Debug, Deserialize)]
struct TopCrates {
    crates: Vec<Crate>,
}

/// The shared description of a crate
#[derive(Debug, Deserialize)]
struct Crate {
    #[serde(rename = "id")]
    name: InternedString,
}

/// A mapping of a crates name to its identifier used in source code
#[derive(Debug, Serialize)]
pub struct CrateInformation {
    pub name: String,
    pub version: Version,
    pub id: String,
}

/// Hand-curated changes to the crate list
#[derive(Debug, Default, Deserialize)]
pub struct Modifications {
    #[serde(default)]
    pub exclusions: Vec<InternedString>,
    #[serde(default)]
    pub additions: BTreeSet<InternedString>,
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "kebab-case")]
pub struct DependencySpec {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub package: String,
    #[serde(serialize_with = "exact_version")]
    pub version: Version,
    #[serde(skip_serializing_if = "BTreeSet::is_empty")]
    pub features: BTreeSet<InternedString>,
    #[serde(skip_serializing_if = "is_true")]
    pub default_features: bool,
}

fn exact_version<S>(version: &Version, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    semver::Comparator {
        op: semver::Op::Exact,
        major: version.major,
        minor: Some(version.minor),
        patch: Some(version.patch),
        pre: version.pre.clone(),
    }
    .serialize(serializer)
}

fn is_true(b: &bool) -> bool {
    *b
}

impl Modifications {
    fn excluded(&self, name: &str) -> bool {
        self.exclusions.iter().any(|n| n == name)
    }
}

fn simple_get(url: &str) -> reqwest::Result<reqwest::blocking::Response> {
    reqwest::blocking::ClientBuilder::new()
        .user_agent("Rust Playground - Top Crates Utility")
        .build()?
        .get(url)
        .send()
}

impl TopCrates {
    /// List top 100 crates by number of downloads on crates.io.
    fn download() -> TopCrates {
        let resp = simple_get("https://crates.io/api/v1/crates?page=1&per_page=100&sort=downloads")
            .expect("Could not fetch top crates");
        assert!(
            resp.status().is_success(),
            "Could not download top crates; HTTP status was {}",
            resp.status(),
        );

        serde_json::from_reader(resp).expect("Invalid JSON")
    }

    fn add_rust_cookbook_crates(&mut self) {
        let mut resp = simple_get(
            "https://raw.githubusercontent.com/rust-lang-nursery/rust-cookbook/master/Cargo.toml",
        )
        .expect("Could not fetch cookbook manifest");
        assert!(
            resp.status().is_success(),
            "Could not download cookbook; HTTP status was {}",
            resp.status(),
        );

        let mut content = String::new();
        resp.read_to_string(&mut content)
            .expect("could not read cookbook manifest");

        let manifest = content
            .parse::<toml::Value>()
            .expect("could not parse cookbook manifest");

        let dependencies = manifest["dependencies"]
            .as_table()
            .expect("no dependencies found for cookbook manifest");
        self.crates.extend({
            dependencies.iter().map(|(name, _)| Crate {
                name: InternedString::new(name),
            })
        })
    }

    /// Add crates that have been hand-picked
    fn add_curated_crates(&mut self, modifications: &Modifications) {
        self.crates.extend({
            modifications
                .additions
                .iter()
                .copied()
                .map(|name| Crate { name })
        });
    }
}

/// Finds the features specified by the custom metadata of `pkg`.
///
/// Our custom metadata format looks like:
///
///     [package.metadata.playground]
///     default-features = true
///     features = ["std", "extra-traits"]
///     all-features = false
///
/// All fields are optional.
fn playground_metadata_features(pkg: &Package) -> Option<(BTreeSet<InternedString>, bool)> {
    let custom_metadata = pkg.manifest().custom_metadata()?;
    let playground_metadata = custom_metadata.get("playground")?;

    #[derive(Deserialize)]
    #[serde(default, rename_all = "kebab-case")]
    struct Metadata {
        features: BTreeSet<InternedString>,
        default_features: bool,
        all_features: bool,
    }

    impl Default for Metadata {
        fn default() -> Self {
            Metadata {
                features: BTreeSet::new(),
                default_features: true,
                all_features: false,
            }
        }
    }

    let metadata = match playground_metadata.clone().try_into::<Metadata>() {
        Ok(metadata) => metadata,
        Err(err) => {
            eprintln!(
                "Failed to parse custom metadata for {} {}: {}",
                pkg.name(),
                pkg.version(),
                err
            );
            return None;
        }
    };

    // If `all-features` is set then we ignore `features`.
    let summary = pkg.summary();
    let mut enabled_features: BTreeSet<InternedString> = if metadata.all_features {
        summary.features().keys().copied().collect()
    } else {
        metadata.features
    };

    // If not opting out of default features, remove default features from the
    // explicit features list. This avoids ongoing spurious diffs in our
    // generated Cargo.toml as default features are added to a library.
    if metadata.default_features {
        if let Some(default_feature_names) = summary.features().get("default") {
            enabled_features.remove("default");
            for feature in default_feature_names {
                enabled_features.remove(&InternedString::new(&feature.to_string()));
            }
        }
    }

    if !enabled_features.is_empty() || !metadata.default_features {
        Some((enabled_features, metadata.default_features))
    } else {
        None
    }
}

fn make_global_state<'cfg>(
    config: &'cfg Config,
    modifications: &'cfg Modifications,
) -> GlobalState<'cfg> {
    // Information about the playground's target platform.
    let compile_target =
        CompileTarget::new(PLAYGROUND_TARGET_PLATFORM).expect("Unable to create a CompileTarget");
    let compile_kind = CompileKind::Target(compile_target);
    let rustc = config
        .load_global_rustc(None)
        .expect("Unable to load the global rustc");
    let target_info = TargetInfo::new(config, &[compile_kind], &rustc, compile_kind)
        .expect("Unable to create a TargetInfo");

    // Registry of known packages.
    let mut registry = PackageRegistry::new(config).expect("Unable to create package registry");
    registry.lock_patches();

    // Source for obtaining packages from the crates.io registry.
    let crates_io = SourceId::crates_io(config).expect("Unable to create crates.io source ID");
    let yanked_whitelist = HashSet::new();
    let mut source = RegistrySource::remote(crates_io, &yanked_whitelist, config)
        .expect("Unable to create registry source");
    source.invalidate_cache();
    source
        .block_until_ready()
        .expect("Unable to wait for registry to be ready");

    GlobalState {
        config,
        target_info,
        registry,
        crates_io,
        source,
        modifications,
    }
}

fn bulk_download(global: &mut GlobalState<'_>, package_ids: &[PackageId]) -> Vec<Package> {
    let mut sources = SourceMap::new();
    sources.insert(Box::new(&mut global.source));

    let package_set = PackageSet::new(package_ids, sources, global.config)
        .expect("Unable to create a PackageSet");

    package_set
        .get_many(package_set.package_ids())
        .expect("Unable to download packages")
        .into_iter()
        .map(Package::clone)
        .collect()
}

fn populate_initial_direct_dependencies(
    global: &mut GlobalState<'_>,
) -> Vec<(Summary, ResolveOpts)> {
    let mut top = TopCrates::download();
    top.add_rust_cookbook_crates();
    top.add_curated_crates(global.modifications);

    // Find the newest (non-prerelease, non-yanked) versions of all
    // the interesting crates.
    let mut summaries = Vec::new();
    for Crate { name } in top.crates {
        if global.modifications.excluded(&name) {
            continue;
        }

        // Query the registry for a summary of this crate.
        // Usefully, this doesn't seem to include yanked versions
        let version = None;
        let dep = Dependency::parse(name, version, global.crates_io)
            .unwrap_or_else(|e| panic!("Unable to parse dependency for {}: {}", name, e));

        let matches = match global.source.query_vec(&dep) {
            Poll::Ready(Ok(v)) => v,
            Poll::Ready(Err(e)) => panic!("Unable to query registry for {}: {}", name, e),
            Poll::Pending => panic!("Registry not ready to query"),
        };

        // Find the newest non-prelease version
        let summary = matches
            .into_iter()
            .filter(|summary| !summary.version().is_prerelease())
            .max_by_key(|summary| summary.version().clone())
            .unwrap_or_else(|| panic!("Registry has no viable versions of {}", name));

        // Add a dependency on this crate.
        summaries.push((
            summary,
            ResolveOpts {
                dev_deps: false,
                features: RequestedFeatures::DepFeatures {
                    features: Default::default(),
                    uses_default_features: true,
                },
            },
        ));
    }

    summaries
}

fn compute_transitive_dependencies(
    global: &mut GlobalState<'_>,
    summaries: Vec<(Summary, ResolveOpts)>,
) -> Vec<PackageId> {
    // Resolve transitive dependencies.
    let replacements = [];
    let version_prefs = VersionPreferences::default();
    let warnings = None;
    let check_public_visible_dependencies = true;
    let resolve = resolver::resolve(
        &summaries,
        &replacements,
        &mut global.registry,
        &version_prefs,
        warnings,
        check_public_visible_dependencies,
    )
    .expect("Unable to resolve dependencies");

    // Find crates incompatible with the playground's platform
    let mut valid_for_our_platform: BTreeSet<_> =
        summaries.iter().map(|(s, _)| s.package_id()).collect();

    let mut to_visit = valid_for_our_platform.clone();

    while !to_visit.is_empty() {
        let mut visit_next = BTreeSet::new();

        for package_id in to_visit {
            for (dep_pkg, deps) in resolve.deps(package_id) {
                let for_this_platform = deps.iter().any(|dep| {
                    dep.platform().map_or(true, |platform| {
                        platform.matches(PLAYGROUND_TARGET_PLATFORM, global.target_info.cfg())
                    })
                });

                if for_this_platform {
                    valid_for_our_platform.insert(dep_pkg);
                    visit_next.insert(dep_pkg);
                }
            }
        }

        to_visit = visit_next;
    }

    // Remove invalid and excluded packages that have been added due to resolution
    resolve
        .iter()
        .filter(|pkg| valid_for_our_platform.contains(pkg))
        .filter(|pkg| !global.modifications.excluded(pkg.name().as_str()))
        .collect()
}

pub fn generate_info(
    modifications: &Modifications,
) -> (BTreeMap<String, DependencySpec>, Vec<CrateInformation>) {
    // Setup to interact with cargo.
    let config = Config::default().expect("Unable to create default Cargo config");
    let _lock = config.acquire_package_cache_lock();
    let mut global = make_global_state(&config, modifications);

    let summaries = populate_initial_direct_dependencies(&mut global);
    let package_ids = compute_transitive_dependencies(&mut global, summaries);
    let packages = bulk_download(&mut global, &package_ids);
    let dependencies = generate_dependency_specs(packages);
    let infos = generate_crate_information(&dependencies);
    (dependencies, infos)
}

fn generate_dependency_specs(mut packages: Vec<Package>) -> BTreeMap<String, DependencySpec> {
    // Sort all packages by name then version (descending), so that
    // when we group them we know we get all the same crates together
    // and the newest version first.
    packages.sort_by(|a, b| {
        a.name()
            .cmp(&b.name())
            .then(a.version().cmp(&b.version()).reverse())
    });

    let mut dependencies = BTreeMap::new();
    for (name, pkgs) in &packages.into_iter().group_by(|pkg| pkg.name()) {
        let mut first = true;

        for pkg in pkgs {
            let version = pkg.version();

            let crate_name = pkg
                .targets()
                .iter()
                .flat_map(|target| match target.kind() {
                    TargetKind::Lib(_) => Some(target.crate_name()),
                    _ => None,
                })
                .next()
                .unwrap_or_else(|| panic!("{} did not have a library", name));

            // We see the newest version first. Any subsequent
            // versions will have their version appended so that they
            // are uniquely named
            let exposed_name = if first {
                crate_name.clone()
            } else {
                format!(
                    "{}_{}_{}_{}",
                    crate_name, version.major, version.minor, version.patch
                )
            };

            let (features, default_features) =
                playground_metadata_features(&pkg).unwrap_or_else(|| (BTreeSet::new(), true));

            dependencies.insert(
                exposed_name.clone(),
                DependencySpec {
                    package: name.to_string(),
                    version: version.clone(),
                    features,
                    default_features,
                },
            );

            first = false;
        }
    }

    dependencies
}

fn generate_crate_information(
    dependencies: &BTreeMap<String, DependencySpec>,
) -> Vec<CrateInformation> {
    let mut infos = Vec::new();

    for (exposed_name, dependency_spec) in dependencies {
        infos.push(CrateInformation {
            name: dependency_spec.package.clone(),
            version: dependency_spec.version.clone(),
            id: exposed_name.clone(),
        });
    }

    infos
}
