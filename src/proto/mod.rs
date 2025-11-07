pub mod runtime {
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/runtime.v1.rs"));
    }
}
