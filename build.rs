fn main() {
    // linkall.x is provided by esp-hal and pulls in the memory layout for the chip.
    println!("cargo:rustc-link-arg=-Tlinkall.x");
}
