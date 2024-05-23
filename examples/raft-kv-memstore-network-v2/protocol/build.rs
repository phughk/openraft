use std::{env, fs};
use std::path::PathBuf;

fn main() {
    // The OUT_DIR is where the generated proto code goes; We display it during builds for debugging
    println!("cargo:warning=OUT_DIR is {}", env::var("OUT_DIR").unwrap());
    tonic_build::configure()
        .compile(&["proto/surrealds/server.proto"], &["proto/surrealds"])
        .unwrap();

    // Get the output directory for generated files
    let out_dir = env::var("OUT_DIR").unwrap();
    let generated_file = PathBuf::from(out_dir).join("surrealds.v1.rs");

    // Define the destination path in the project source directory
    let dest_dir = PathBuf::from("src/proto/surrealds");
    let dest_file = dest_dir.join("v1.rs");

    // Create the destination directory if it doesn't exist
    fs::create_dir_all(&dest_dir).unwrap();

    // Copy the generated file to the project source directory
    fs::copy(generated_file, dest_file).unwrap();
}
