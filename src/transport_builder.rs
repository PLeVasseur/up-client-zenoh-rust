use async_std::task;
use crate::UPClientSomeipstandin;
use up_rust::transport::datamodel::UTransport;
use ustreamer::utransport_builder::UTransportBuilder;
use zenoh::prelude::Config;

pub struct SomeipstandinTransportBuilder {
    config: Option<Config>,
}

impl SomeipstandinTransportBuilder {
    #[allow(dead_code)]
    pub fn new_with_config(config: Config) -> Self {
        Self {
            config: Some(config),
        }
    }

    #[allow(dead_code)]
    pub fn new() -> Self {
        Self { config: None }
    }
}

impl UTransportBuilder for SomeipstandinTransportBuilder {
    fn build(&self) -> Box<dyn UTransport> {
        let config = self.config.clone().unwrap_or_default();
        let utransport: Box<dyn UTransport> = Box::new(
            task::block_on(UPClientSomeipstandin::new(config)).expect("Unable to create UPClientSomeipstandin!"),
        );
        utransport
    }
}