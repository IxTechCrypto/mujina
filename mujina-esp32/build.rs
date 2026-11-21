fn main() {
    #[cfg(feature = "esp32s3")]
    embuild::espidf::sysenv::output();
}
