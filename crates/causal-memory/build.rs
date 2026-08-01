fn main() {
    // On macOS, ONNX Runtime (ort-sys) references CoreML symbols.
    // Link the CoreML framework to resolve them.
    #[cfg(target_os = "macos")]
    {
        println!("cargo:rustc-link-lib=framework=CoreML");
    }
}
