extern crate cargo_metadata;

use cmake_config::{Key, SimpleFlag};
use command_ext::CommandExt;
use fel4_config::{FlatTomlValue, SupportedTarget};
use std::borrow::Borrow;
use std::collections::HashSet;
use std::env::{self, current_dir};
use std::fs::{self, canonicalize, File};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::Error;
use cmake_codegen::{cache_to_interesting_flags, truthy_boolean_flags_as_rust_identifiers};
use config::{get_resolved_config, BuildCmd, Fel4BuildProfile, ResolvedConfig};
use generator::Generator;

pub fn handle_build_cmd(subcmd: &BuildCmd) -> Result<(), Error> {
    let build_profile = Fel4BuildProfile::from(subcmd);
    let config: ResolvedConfig = get_resolved_config(&subcmd.cargo_manifest_path, &build_profile)?;

    let artifact_path = &config
        .root_dir
        .join(&config.fel4_config.artifact_path)
        .join(build_profile.artifact_subdir_path());

    let target_build_cache_path = config
        .root_dir
        .join("target")
        .join(config.fel4_config.target.full_name())
        .join(build_profile.as_fel4_config_build_profile().full_name());

    info!("\ntarget build cache: {:?}", target_build_cache_path,);

    let cross_layer_locations = CrossLayerLocations {
        fel4_artifact_path: config.root_dir.join(&artifact_path),
        fel4_manifest_path: config.root_dir.join("fel4.toml"),
        rust_target_path: config.root_dir.join(&config.fel4_config.target_specs_path),
    };

    let fel4_flags: Vec<SimpleFlag> = config
        .fel4_config
        .properties
        .iter()
        .map(|(k, v): (&String, &FlatTomlValue)| {
            let key = Key(k.to_string());
            match v {
                FlatTomlValue::Boolean(b) => SimpleFlag::Boolish(key, *b),
                FlatTomlValue::String(s) => SimpleFlag::Stringish(key, s.to_string()),
                FlatTomlValue::Integer(s) => SimpleFlag::Stringish(key, s.to_string()),
                FlatTomlValue::Float(s) => SimpleFlag::Stringish(key, s.to_string()),
                FlatTomlValue::Datetime(s) => SimpleFlag::Stringish(key, s.to_string()),
            }
        })
        .collect();
    let rustflags_env_var = merge_feature_flags_with_rustflags_env_var(
        &truthy_boolean_flags_as_rust_identifiers(&fel4_flags)?,
    );

    // Generate the source code entry point (root task) for the application
    // that will wrap the end-user's code as executing within a sub-thread
    let root_task_path = config.root_dir.join("src").join("bin");
    fs::create_dir_all(&root_task_path).map_err(|e| {
        Error::IO(format!(
            "Difficulty creating directory, {:?} : {}",
            &root_task_path, e
        ))
    })?;
    let mut root_file = File::create(root_task_path.join("root-task.rs").as_path())
        .map_err(|e| Error::IO(format!("Could not create root-task file. {}", e)))?;
    Generator::new(
        &mut root_file,
        &config.pkg_module_name,
        &config.arch,
        &fel4_flags,
    ).generate()?;

    match is_current_dir_root_dir(&config.root_dir) {
        Ok(are_same) if !are_same => return Err(Error::ExitStatusError("The build command does not work with a cargo manifest directory that differs from the current working directory due to limitations of Xargo".to_string())),
        Err(e) => return Err(Error::IO(format!("Error with current dir comparison: {}", e))),
        _ => ()
    }
    // Build the generated root task binary
    construct_root_task_build_command(subcmd, &config, &cross_layer_locations)
        .env("RUSTFLAGS", &rustflags_env_var)
        .run_cmd()?;

    let sysimg_path = artifact_path.join("feL4img");
    let kernel_path = artifact_path.join("kernel");
    fs::create_dir_all(&artifact_path)?;

    // For ARM targets, we currently take advantage of the
    // seL4 elfloader-tool to bootstrap the system and kick
    // things off.
    // To accomplish this, we just re-build libsel4-sys
    // with an extra environment variable which gives
    // elfloader-tool a path to the root-task binary
    match config.fel4_config.target {
        SupportedTarget::Armv7Sel4Fel4 => {
            construct_libsel4_build_command(subcmd, &config, &cross_layer_locations)
                .env(
                    "FEL4_ROOT_TASK_IMAGE_PATH",
                    target_build_cache_path.join("root-task"),
                )
                .env("RUSTFLAGS", &rustflags_env_var)
                .run_cmd()?;

            // seL4 CMake rules will just output everything to `kernel`
            // we copy it so it's consistent with our image name but
            // won't trigger a rebuild (as it would if we were to move it)
            fs::copy(&kernel_path, &sysimg_path)?;
        }
        SupportedTarget::Aarch64Sel4Fel4 => {
            construct_libsel4_build_command(subcmd, &config, &cross_layer_locations)
                .env(
                    "FEL4_ROOT_TASK_IMAGE_PATH",
                    target_build_cache_path.join("root-task"),
                )
                .env("RUSTFLAGS", &rustflags_env_var)
                .run_cmd()?;

            // seL4 CMake rules will just output everything to `kernel`
            // we copy it so it's consistent with our image name but
            // won't trigger a rebuild (as it would if we were to move it)
            fs::copy(&kernel_path, &sysimg_path)?;
        }
        _ => {
            fs::copy(target_build_cache_path.join("root-task"), &sysimg_path)?;
        }
    }

    {
        // Extract the resolved CMake config details and filter down to ones that might
        // be useful for cross-reference with the fel4-config derived values
        let interesting_raw_flags_from_cmake = cache_to_interesting_flags(
            config.root_dir.join(&artifact_path).join("CMakeCache.txt"),
        )?;
        let simple_cmake_flags: HashSet<SimpleFlag> = interesting_raw_flags_from_cmake
            .iter()
            .map(SimpleFlag::from)
            .collect();
        let simple_fel4_flags: HashSet<SimpleFlag> = fel4_flags.into_iter().collect();
        if !&simple_fel4_flags.is_subset(&simple_cmake_flags) {
            for s in &simple_fel4_flags {
                if simple_cmake_flags.contains(s) {
                    continue;
                }
                println!("Found a fel4 flag {:?} that was not in the cmake flags", s);
                let key = match s {
                    SimpleFlag::Boolish(Key(k), _) | SimpleFlag::Stringish(Key(k), _) => k.clone(),
                };
                for raw_flag in &interesting_raw_flags_from_cmake {
                    if raw_flag.key == key {
                        println!(
                            "    But there was a flag with the same key in CMakeCache.txt: {:?}",
                            raw_flag
                        );
                    }
                }
            }
            return Err(Error::ConfigError("Unexpected mismatch between the fel4.toml config values and seL4's CMakeCache.txt config values".to_string()));
        }
    }

    if !sysimg_path.exists() {
        return Err(Error::ConfigError(format!(
            "Something went wrong with the build, cannot find the system image '{}'",
            target_build_cache_path.join(&sysimg_path).display()
        )));
    }

    if !kernel_path.exists() {
        return Err(Error::ConfigError(format!(
            "Something went wrong with the build, cannot find the kernel file '{}'",
            kernel_path.display()
        )));
    }

    info!("Output artifact path '{}'", artifact_path.display());

    info!("kernel: '{}'", kernel_path.display());
    info!("feL4img: '{}'", sysimg_path.display());

    Ok(())
}

fn is_current_dir_root_dir<P: AsRef<Path>>(root_dir: P) -> Result<bool, ::std::io::Error> {
    let root_dir_buf: PathBuf = root_dir.as_ref().into();
    Ok(canonicalize(root_dir_buf)? == canonicalize(current_dir()?)?)
}

fn construct_libsel4_build_command<P>(
    subcmd: &BuildCmd,
    config: &ResolvedConfig,
    locations: &CrossLayerLocations<P>,
) -> Command
where
    P: Borrow<Path>,
{
    let mut libsel4_build = Command::new("xargo");

    libsel4_build
        .arg("rustc")
        .arg("--manifest-path")
        .arg(&subcmd.cargo_manifest_path)
        .arg_if(|| subcmd.release, "--release")
        .add_loudness_args(&subcmd.loudness)
        .handle_arm_edge_case(&config.fel4_config.target)
        .add_locations_as_env_vars(locations)
        .arg("--target")
        .arg(&config.fel4_config.target.full_name())
        .arg("-p")
        .arg("libsel4-sys");

    libsel4_build
}

/// Create a Command instance that, when run,
/// will build the root task binary
///
/// Note: Does NOT include application of Rust/Cargo feature flags
///
/// TODO: Replace our optional dependency usage with proper
/// test feature flagging when custom test frameworks are
/// more feasible in our environment
fn construct_root_task_build_command<P>(
    subcmd: &BuildCmd,
    config: &ResolvedConfig,
    cross_layer_locations: &CrossLayerLocations<P>,
) -> Command
where
    P: Borrow<Path>,
{
    let mut root_task_build = Command::new("xargo");
    root_task_build
        .arg("rustc")
        .arg("--bin")
        .arg("root-task")
        .arg("--manifest-path")
        .arg(&subcmd.cargo_manifest_path)
        .arg_if(|| subcmd.release, "--release")
        .add_loudness_args(&subcmd.loudness)
        .handle_arm_edge_case(&config.fel4_config.target)
        .arg_if(|| subcmd.tests, "--features")
        .arg_if(|| subcmd.tests, "test alloc")
        .arg("--target")
        .arg(&config.fel4_config.target.full_name())
        .add_locations_as_env_vars(cross_layer_locations);
    root_task_build
}

/// Common-cause struct for the path data associated with the environment
/// variables used by cargo-fel4 to communicate across package and process
/// boundaries.
#[derive(Clone, Debug, PartialEq)]
pub struct CrossLayerLocations<P: Borrow<Path>> {
    fel4_manifest_path: P,
    fel4_artifact_path: P,
    rust_target_path: P,
}

/// Extension methods for `Command` instances to supply common parameters or
/// metadata
trait BuildCommandExt
where
    Self: Into<Command>,
{
    /// Populate the command with the environment variables tracked by
    /// CrossLayerLocations
    fn add_locations_as_env_vars<'c, 'l, P: Borrow<Path>>(
        &'c mut self,
        cross_layer_locations: &'l CrossLayerLocations<P>,
    ) -> &'c mut Self;

    /// Handle a possible edge case in cross-compiling for arm
    fn handle_arm_edge_case<'c, 'f>(&'c mut self, config: &'f SupportedTarget) -> &'c mut Self;
}

impl BuildCommandExt for Command {
    fn add_locations_as_env_vars<'c, 'l, P: Borrow<Path>>(
        &'c mut self,
        locations: &'l CrossLayerLocations<P>,
    ) -> &'c mut Self {
        self.env("FEL4_MANIFEST_PATH", locations.fel4_manifest_path.borrow())
            .env("FEL4_ARTIFACT_PATH", locations.fel4_artifact_path.borrow())
            .env("RUST_TARGET_PATH", locations.rust_target_path.borrow());
        self
    }

    fn handle_arm_edge_case<'c, 'f>(&'c mut self, target: &'f SupportedTarget) -> &mut Self {
        // There seems to be an issue with `compiler_builtins` imposing
        // a default compiler used by the `c` feature/dependency; where
        // it no longer picks up a sane cross-compiler (when host != target triple).
        // This results in compiler_builtin_shims being compiled with the
        // host's default compiler (likely x86_64) rather than using
        // what our target specification (or even Xargo.toml) has prescribed.
        //
        // This fix is a band aid, and will be addressed properly at a later point.
        // However we can still force/control which cross compiler will
        // get used to build the shims through the use of CC's envirnoment
        // variables.
        //
        // See the following issues:
        // `xargo/issues/216`
        // `cargo-fel4/issues/18`
        match *target {
            SupportedTarget::Armv7Sel4Fel4 => {
                self.env("CC_armv7-sel4-fel4", "arm-linux-gnueabihf-gcc")
            }
            SupportedTarget::Aarch64Sel4Fel4 => {
                self.env("CC_aarch64-sel4-fel4", "aarch64-linux-gnu-gcc")
            }
            _ => self,
        }
    }
}

fn merge_feature_flags_with_rustflags_env_var(feature_flags: &[String]) -> String {
    let mut output: String = match env::var("RUSTFLAGS") {
        Ok(s) => s,
        Err(env::VarError::NotUnicode(_)) => String::new(),
        Err(env::VarError::NotPresent) => String::new(),
    };
    if !output.is_empty() {
        output.push(' ');
    }
    for feature in feature_flags {
        output.push_str("--cfg ");
        output.push_str(&format!("feature=\"{}\" ", feature));
    }
    output
}
