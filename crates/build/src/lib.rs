mod docker;

use anyhow::{Context, Result};
use cargo_metadata::camino::Utf8PathBuf;
use clap::Parser;
use dirs::home_dir;
use std::{
    env, fs,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{exit, Command, Stdio},
    thread,
};

const BUILD_TARGET: &str = "riscv32im-succinct-zkvm-elf";
const DEFAULT_TAG: &str = "v1.1.0";
const DEFAULT_OUTPUT_DIR: &str = "elf";
const HELPER_TARGET_SUBDIR: &str = "elf-compilation";

/// Compile an SP1 program.
///
/// Additional arguments are useful for configuring the build process, including options for using
/// Docker, specifying binary and ELF names, ignoring Rust version checks, and enabling specific
/// features.
#[derive(Clone, Parser, Debug)]
pub struct BuildArgs {
    #[clap(
        long,
        action,
        help = "Run compilation using a Docker container for reproducible builds."
    )]
    pub docker: bool,
    #[clap(
        long,
        help = "The ghcr.io/succinctlabs/sp1 image tag to use when building with Docker.",
        default_value = DEFAULT_TAG
    )]
    pub tag: String,
    #[clap(
        long,
        action,
        value_delimiter = ',',
        help = "Space or comma separated list of features to activate"
    )]
    pub features: Vec<String>,
    #[clap(long, action, help = "Do not activate the `default` feature")]
    pub no_default_features: bool,
    #[clap(long, action, help = "Ignore `rust-version` specification in packages")]
    pub ignore_rust_version: bool,
    #[clap(long, action, help = "Assert that `Cargo.lock` will remain unchanged")]
    pub locked: bool,
    #[clap(
        alias = "bin",
        long,
        action,
        help = "Build only the specified binary",
        default_value = ""
    )]
    pub binary: String,
    #[clap(long, action, help = "ELF binary name", default_value = "")]
    pub elf_name: String,
    #[clap(
        alias = "out-dir",
        long,
        action,
        help = "Copy the compiled ELF to this directory",
        default_value = DEFAULT_OUTPUT_DIR
    )]
    pub output_directory: String,
}

// Implement default args to match clap defaults.
impl Default for BuildArgs {
    fn default() -> Self {
        Self {
            docker: false,
            tag: DEFAULT_TAG.to_string(),
            features: vec![],
            ignore_rust_version: false,
            binary: "".to_string(),
            elf_name: "".to_string(),
            output_directory: DEFAULT_OUTPUT_DIR.to_string(),
            locked: false,
            no_default_features: false,
        }
    }
}

/// Get the arguments to build the program with the arguments from the [`BuildArgs`] struct.
fn get_program_build_args(args: &BuildArgs) -> Vec<String> {
    let mut build_args = vec![
        "build".to_string(),
        "--release".to_string(),
        "--target".to_string(),
        BUILD_TARGET.to_string(),
    ];

    if args.ignore_rust_version {
        build_args.push("--ignore-rust-version".to_string());
    }

    if !args.binary.is_empty() {
        build_args.push("--bin".to_string());
        build_args.push(args.binary.clone());
    }

    if !args.features.is_empty() {
        build_args.push("--features".to_string());
        build_args.push(args.features.join(","));
    }

    if args.no_default_features {
        build_args.push("--no-default-features".to_string());
    }

    if args.locked {
        build_args.push("--locked".to_string());
    }

    build_args
}

/// Rust flags for compilation of C libraries.
fn get_rust_compiler_flags() -> String {
    let rust_flags = [
        "-C".to_string(),
        "passes=loweratomic".to_string(),
        "-C".to_string(),
        "link-arg=-Ttext=0x00200800".to_string(),
        "-C".to_string(),
        "panic=abort".to_string(),
    ];
    rust_flags.join("\x1f")
}

/// Get the command to build the program locally.
fn create_local_command(
    args: &BuildArgs,
    program_dir: &Utf8PathBuf,
    program_metadata: &cargo_metadata::Metadata,
) -> Command {
    let mut command = Command::new("cargo");
    let canonicalized_program_dir =
        program_dir.canonicalize().expect("Failed to canonicalize program directory");

    // If CC_riscv32im_succinct_zkvm_elf is not set, set it to the default C++ toolchain
    // downloaded by 'sp1up --c-toolchain'.
    if env::var("CC_riscv32im_succinct_zkvm_elf").is_err() {
        if let Some(home_dir) = home_dir() {
            let cc_path = home_dir.join(".sp1").join("bin").join("riscv32-unknown-elf-gcc");
            if cc_path.exists() {
                command.env("CC_riscv32im_succinct_zkvm_elf", cc_path);
            }
        }
    }

    // When executing the local command:
    // 1. Set the target directory to a subdirectory of the program's target directory to avoid
    //    build
    // conflicts with the parent process. Source: https://github.com/rust-lang/cargo/issues/6412
    // 2. Set the rustup toolchain to succinct.
    // 3. Set the encoded rust flags.
    // 4. Remove the rustc configuration, otherwise in a build script it will attempt to compile the
    //    program with the toolchain of the normal build process, rather than the Succinct
    //    toolchain.
    command
        .current_dir(canonicalized_program_dir)
        .env("RUSTUP_TOOLCHAIN", "succinct")
        .env("CARGO_ENCODED_RUSTFLAGS", get_rust_compiler_flags())
        .env_remove("RUSTC")
        .env("CARGO_TARGET_DIR", program_metadata.target_directory.join(HELPER_TARGET_SUBDIR))
        .args(&get_program_build_args(args));
    command
}

/// Execute the command and handle the output depending on the context.
fn execute_command(mut command: Command, docker: bool) -> Result<()> {
    // Add necessary tags for stdout and stderr from the command.
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to spawn command")?;
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let stderr = BufReader::new(child.stderr.take().unwrap());

    // Add prefix to the output of the process depending on the context.
    let msg = match docker {
        true => "[sp1] [docker] ",
        false => "[sp1] ",
    };

    // Pipe stdout and stderr to the parent process with [docker] prefix
    let stdout_handle = thread::spawn(move || {
        stdout.lines().for_each(|line| {
            println!("{} {}", msg, line.unwrap());
        });
    });
    stderr.lines().for_each(|line| {
        eprintln!("{} {}", msg, line.unwrap());
    });
    stdout_handle.join().unwrap();

    // Wait for the child process to finish and check the result.
    let result = child.wait()?;
    if !result.success() {
        // Error message is already printed by cargo.
        exit(result.code().unwrap_or(1))
    }
    Ok(())
}

/// Copy the ELF to the specified output directory.
fn copy_elf_to_output_dir(
    args: &BuildArgs,
    program_metadata: &cargo_metadata::Metadata,
) -> Result<Utf8PathBuf> {
    let root_package = program_metadata.root_package();
    let root_package_name = root_package.as_ref().map(|p| &p.name);

    // The ELF is written to a target folder specified by the program's package. If built with
    // Docker, includes /docker after HELPER_TARGET_SUBDIR.
    let mut target_dir_suffix = HELPER_TARGET_SUBDIR.to_string();
    if args.docker {
        target_dir_suffix = format!("{}/{}", HELPER_TARGET_SUBDIR, "docker");
    }

    // The ELF's file name is the binary name if it's specified. Otherwise, it is the root package
    // name.
    let original_elf_file_name = if !args.binary.is_empty() {
        args.binary.clone()
    } else {
        root_package_name.unwrap().clone()
    };

    let original_elf_path = program_metadata
        .target_directory
        .join(target_dir_suffix)
        .join(BUILD_TARGET)
        .join("release")
        .join(original_elf_file_name);

    // The order of precedence for the ELF name is:
    // 1. --elf_name flag
    // 2. --binary flag + -elf suffix (defaults to riscv32im-succinct-zkvm-elf)
    let elf_name = if !args.elf_name.is_empty() {
        args.elf_name.clone()
    } else if !args.binary.is_empty() {
        // TODO: In the future, change this to default to the package name. Will require updating
        // docs and examples.
        args.binary.clone()
    } else {
        BUILD_TARGET.to_string()
    };

    let elf_dir = program_metadata.target_directory.parent().unwrap().join(&args.output_directory);
    fs::create_dir_all(&elf_dir)?;
    let result_elf_path = elf_dir.join(elf_name);

    // Copy the ELF to the specified output directory.
    fs::copy(original_elf_path, &result_elf_path)?;

    Ok(result_elf_path)
}

/// Build a program with the specified [`BuildArgs`]. The `program_dir` is specified as an argument
/// when the program is built via `build_program` in sp1-helper.
///
/// # Arguments
///
/// * `args` - A reference to a `BuildArgs` struct that holds various arguments used for building
///   the program.
/// * `program_dir` - An optional `PathBuf` specifying the directory of the program to be built.
///
/// # Returns
///
/// * `Result<Utf8PathBuf>` - The path to the built program as a `Utf8PathBuf` on success, or an
///   error on failure.
pub fn build_program(args: &BuildArgs, program_dir: Option<PathBuf>) -> Result<Utf8PathBuf> {
    // If the program directory is not specified, use the current directory.
    let program_dir = program_dir
        .unwrap_or_else(|| std::env::current_dir().expect("Failed to get current directory."));
    let program_dir: Utf8PathBuf =
        program_dir.try_into().expect("Failed to convert PathBuf to Utf8PathBuf");

    // Get the program metadata.
    let program_metadata_file = program_dir.join("Cargo.toml");
    let mut program_metadata_cmd = cargo_metadata::MetadataCommand::new();
    let program_metadata =
        program_metadata_cmd.manifest_path(program_metadata_file).exec().unwrap();

    // Get the command corresponding to Docker or local build.
    let cmd = if args.docker {
        docker::create_docker_command(args, &program_dir, &program_metadata)?
    } else {
        create_local_command(args, &program_dir, &program_metadata)
    };

    execute_command(cmd, args.docker)?;

    copy_elf_to_output_dir(args, &program_metadata)
}
