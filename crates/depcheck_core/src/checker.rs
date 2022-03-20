use std::collections::{BTreeMap, HashSet};
use std::iter;
use std::path::{Component, PathBuf};

use ignore::overrides::OverrideBuilder;
use ignore::{self, WalkBuilder};
use relative_path::RelativePathBuf;
use swc_common::comments::SingleThreadedComments;
use swc_ecma_dep_graph::{analyze_dependencies, DependencyKind};
use swc_ecma_parser::Syntax;

use crate::config::Config;
use crate::package::{self, DepsSet, Package};
use crate::parser::Parser;
use crate::util::extract_package_name::extract_package_name;
use crate::util::extract_type_name::extract_type_name;
use crate::util::is_bin_dependency::is_bin_dependency;
use crate::util::is_core_module::is_core_module;
use crate::util::is_module::is_module;
use crate::util::load_module::load_module;

pub struct Checker {
    config: Config,
    parsers: Parser,
}

impl Checker {
    pub fn new(config: Config) -> Self {
        Checker {
            config,
            parsers: Default::default(),
        }
    }
}

impl Checker {
    pub fn check_package(&self) -> package::Result<CheckResult> {
        let package = load_module(self.config.get_directory())?;

        let dependencies = self.check_directory(&package);

        let mut using_dependencies = BTreeMap::new();

        dependencies.into_iter().for_each(|(path, dependencies)| {
            dependencies.iter().for_each(|dependency| {
                let files = using_dependencies
                    .entry(dependency.clone())
                    .or_insert_with(|| HashSet::with_capacity(100));
                files.insert(path.clone());
            })
        });

        Ok(CheckResult {
            package,
            directory: self.config.get_directory().to_path_buf(),
            using_dependencies,
            config: self.config.clone(),
        })
    }

    pub fn check_directory(&self, package: &Package) -> BTreeMap<RelativePathBuf, HashSet<String>> {
        let directory = self.config.get_directory();
        let comments = SingleThreadedComments::default();
        let mut override_builder = OverrideBuilder::new(directory);

        for pattern in self.config.get_ignore_patterns() {
            override_builder
                .add(&format!("!{pattern}"))
                .map_err(|e| format!("Malformed exclude pattern: {e}"))
                .unwrap();
        }

        let overrides = override_builder
            .build()
            .expect("Mismatch in exclude patterns");
        let mut walker = WalkBuilder::new(directory);

        walker.overrides(overrides).filter_entry(move |entry| {
            let is_root_directory = entry.depth() == 0;
            is_root_directory || !is_module(entry.path())
        });

        if self.config.read_depcheckignore() {
            walker.add_custom_ignore_filename(".depcheckignore");
        }

        let walker = walker.build();

        walker
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| match entry.file_type() {
                Some(file_type) => file_type.is_file(),
                _ => false,
            })
            .filter_map(|file| {
                let path = file.path().strip_prefix(directory).ok()?;
                let relative_file_path = RelativePathBuf::from_path(path).ok()?;
                self.parsers
                    .parse_file(file.path())
                    .map(|(module, syntax)| (relative_file_path, module, syntax))
            })
            .map(|(relative_file_path, module, syntax)| {
                let file_dependencies = analyze_dependencies(&module, &comments);
                let file_dependencies = file_dependencies
                    .iter()
                    .filter(|dependency| {
                        let path = PathBuf::from(&dependency.specifier.to_string());
                        let root = path.components().next();

                        matches!(root, Some(Component::Normal(_)))
                    })
                    .flat_map(|dependency| {
                        let name = extract_package_name(&dependency.specifier).unwrap();

                        match syntax {
                            Syntax::Typescript(_) => {
                                if dependency.kind == DependencyKind::ImportType {
                                    let type_dependency = "@types/".to_string() + &name;
                                    return if package.is_dependency(&type_dependency)
                                        || package.is_dev_dependency(&type_dependency)
                                    {
                                        vec![type_dependency]
                                    } else {
                                        vec![]
                                    };
                                }
                                let type_dependency = extract_type_name(&name);
                                if package.is_dependency(&type_dependency)
                                    || package.is_dev_dependency(&type_dependency)
                                {
                                    return vec![name, type_dependency];
                                }
                                vec![name]
                            }
                            _ => vec![name],
                        }
                    })
                    .filter(|dependency| !is_core_module(dependency))
                    .filter(|dependency| {
                        !self.config.ignore_bin_package()
                            || !is_bin_dependency(directory, dependency)
                    })
                    .flat_map(|dependency| {
                        let dependency_module =
                            load_module(&directory.join("node_modules").join(&dependency));
                        let dependencies = match dependency_module {
                            Ok(dependency_module) => iter::once(dependency)
                                .chain(
                                    dependency_module
                                        .peer_dependencies
                                        .keys()
                                        .filter(|&peer_dependency| {
                                            package.is_dependency(peer_dependency)
                                                || package.is_dev_dependency(peer_dependency)
                                        })
                                        .cloned(),
                                )
                                .chain(
                                    dependency_module
                                        .optional_dependencies
                                        .keys()
                                        .filter(|&optional_dependency| {
                                            package.is_dependency(optional_dependency)
                                                || package.is_dev_dependency(optional_dependency)
                                        })
                                        .cloned(),
                                )
                                .collect(),
                            Err(_) => {
                                vec![dependency]
                            }
                        };

                        dependencies
                    })
                    .collect();

                (relative_file_path, file_dependencies)
            })
            .collect()
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct CheckResult {
    pub package: Package,
    pub directory: PathBuf,
    pub using_dependencies: BTreeMap<String, HashSet<RelativePathBuf>>,
    pub config: Config,
}

impl CheckResult {
    pub fn get_missing_dependencies(&self) -> BTreeMap<&str, &HashSet<RelativePathBuf>> {
        if self.config.skip_missing() {
            Default::default()
        } else {
            let ignore_matches = self
                .config
                .get_ignore_matches()
                .expect("Can't get ignore matches");
            self.using_dependencies
                .iter()
                .filter(|(dependency, _)| !ignore_matches.is_match(dependency.as_str()))
                .filter(|(dependency, _)| !self.package.is_any_dependency(dependency))
                .filter(|(dependency, _)| {
                    !self.config.ignore_bin_package()
                        || !is_bin_dependency(&self.directory, dependency)
                })
                .map(|(dependency, files)| (dependency.as_str(), files))
                .collect()
        }
    }

    pub fn get_unused_dependencies(&self) -> HashSet<&str> {
        self.filter_unused_dependencies(&self.package.dependencies)
    }

    pub fn get_unused_dev_dependencies(&self) -> HashSet<&str> {
        self.filter_unused_dependencies(&self.package.dev_dependencies)
    }

    fn filter_unused_dependencies<'a>(&self, dependencies: &'a DepsSet) -> HashSet<&'a str> {
        let ignore_matches = self
            .config
            .get_ignore_matches()
            .expect("Can't get ignore matches");

        dependencies
            .iter()
            .filter(|(dependency, _)| !ignore_matches.is_match(dependency.as_str()))
            .filter(|(dependency, _)| !self.using_dependencies.contains_key(dependency.as_str()))
            .filter(|(dependency, _)| {
                !self.config.ignore_bin_package() || !is_bin_dependency(&self.directory, dependency)
            })
            .map(|(dependency, _)| dependency.as_str())
            .collect()
    }
}
