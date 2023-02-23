pub mod envoy {
    pub mod r#type {
        tonic::include_proto!("envoy.r#type.v3");

        pub mod v3 {
            tonic::include_proto!("envoy.r#type.v3");
        }
    }

    pub mod config {
        pub mod core {
            pub mod v3 {
                tonic::include_proto!("envoy.config.core.v3");
            }
        }
    }

    pub mod extensions {
        pub mod common {
            pub mod ratelimit {
                pub mod v3 {
                    tonic::include_proto!("envoy.extensions.common.ratelimit.v3");
                }
            }
        }
    }

    pub mod service {
        pub mod ratelimit {
            pub mod v3 {
                tonic::include_proto!("envoy.service.ratelimit.v3");
            }
        }
    }
}

// pub mod google {
//     pub mod rpc {
//         tonic::include_proto!("google.rpc");
//     }
// }

pub mod xds {
    pub mod core {
        pub mod v3 {
            tonic::include_proto!("xds.core.v3");
        }
    }
}
