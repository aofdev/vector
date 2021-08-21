use crate::config::{DataType, GenerateConfig, SinkConfig, SinkContext};
use crate::http::HttpClient;
use crate::sinks::datadog::logs::healthcheck::healthcheck;
use crate::sinks::datadog::logs::service;
use crate::sinks::datadog::logs::sink::LogApi;
use crate::sinks::datadog::ApiKey;
use crate::sinks::datadog::Region;
use crate::sinks::util::encoding::EncodingConfigWithDefault;
use crate::sinks::util::{
    batch::{Batch, BatchError},
    http::HttpSink,
    BatchConfig, BatchSettings, Compression, JsonArrayBuffer, PartitionInnerBuffer,
    TowerRequestConfig,
};
use crate::sinks::{Healthcheck, VectorSink};
use crate::tls::{MaybeTlsSettings, TlsConfig};
use futures::FutureExt;
use indoc::indoc;
use serde::{Deserialize, Serialize};
use std::convert::TryFrom;
use std::{sync::Arc, time::Duration};

// What is important here? I have to have a solution that satisfies the
// `BatchConfig` passed in. The final sink-type thing must fit into a
// `VectorSink`. The `BatchConfig` has two important features:
//
// * `max_bytes` -- the total number of bytes to batch up before processing
// * `timeout_secs` -- the total number of seconds to store events maximum before processing
//
// This sink does not obey `max_events`.
//
// Now, the datadog logs endpoint has some limitations to be aware of. First, a
// payload is defined by API key. That is, ultimately, if I have events
// associated with N API keys even if the sum of all the events is under the
// logs API payload limit I have N payloads to get out. Also, a payload may be
// no more than 5Mb and the interior array may have no more than 1000
// members. There is no limit on the number of requests that can be made in a
// given interval of time.
//
// Okay, so, for each API key buffer up to 1_000 events under that API _or_ in
// the event of a timeout (global) _or_ in the event that the size of the
// buffered events exceeds `max_bytes` (global) kick out a request to the
// datadog API. We will take the byte size of the `Event` -- since that's what
// impacts the user -- and NOT its serialized size. As such we'll have to take
// care to avoid serialization that goes over the prescibed size limit to the
// API.

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct DatadogLogsConfig {
    pub(crate) endpoint: Option<String>,
    // Deprecated, replaced by the site option
    region: Option<Region>,
    site: Option<String>,
    // Deprecated name
    #[serde(alias = "api_key")]
    default_api_key: String,
    #[serde(
        skip_serializing_if = "crate::serde::skip_serializing_if_default",
        default
    )]
    pub(crate) encoding: EncodingConfigWithDefault<Encoding>,
    tls: Option<TlsConfig>,

    #[serde(default)]
    compression: Option<Compression>,

    #[serde(default)]
    batch: BatchConfig,

    #[serde(default)]
    request: TowerRequestConfig,
}

#[derive(Deserialize, Serialize, Debug, Eq, PartialEq, Clone, Derivative)]
#[serde(rename_all = "snake_case")]
#[derivative(Default)]
pub enum Encoding {
    #[derivative(Default)]
    Json,
}

impl GenerateConfig for DatadogLogsConfig {
    fn generate_config() -> toml::Value {
        toml::from_str(indoc! {r#"
            default_api_key = "${DATADOG_API_KEY_ENV_VAR}"
        "#})
        .unwrap()
    }
}

impl DatadogLogsConfig {
    fn get_uri(&self) -> http::Uri {
        let endpoint = self
            .endpoint
            .clone()
            .or_else(|| {
                self.site
                    .as_ref()
                    .map(|s| format!("https://http-intake.logs.{}/v1/input", s))
            })
            .unwrap_or_else(|| match self.region {
                Some(Region::Eu) => "https://http-intake.logs.datadoghq.eu/v1/input".to_string(),
                None | Some(Region::Us) => {
                    "https://http-intake.logs.datadoghq.com/v1/input".to_string()
                }
            });
        http::Uri::try_from(endpoint).expect("URI not valid")
    }

    fn batch_settings<T: Batch>(&self) -> Result<BatchSettings<T>, BatchError> {
        BatchSettings::default()
            .bytes(bytesize::mib(5_u32))
            .events(1_000)
            .timeout(15)
            .parse_config(self.batch)
    }

    // /// Builds the required BatchedHttpSink.
    // /// Since the DataDog sink can create one of two different sinks, this
    // /// extracts most of the shared functionality required to create either sink.
    // fn build_sink<T, B, O>(
    //     &self,
    //     cx: SinkContext,
    //     service: T,
    //     _batch: B,
    //     timeout: Duration,
    // ) -> crate::Result<(VectorSink, Healthcheck)>
    // where
    //     O: 'static,
    //     B: Batch<Output = Vec<O>> + std::marker::Send + 'static,
    //     B::Output: std::marker::Send + Clone,
    //     B::Input: std::marker::Send,
    //     T: HttpSink<
    //             Input = PartitionInnerBuffer<B::Input, ApiKey>,
    //             Output = PartitionInnerBuffer<B::Output, ApiKey>,
    //         > + Clone,
    // {
    //     // let request_settings = self.request.unwrap_with(&TowerRequestConfig::default());

    //     let tls_settings = MaybeTlsSettings::from_config(
    //         &Some(self.tls.clone().unwrap_or_else(TlsConfig::enabled)),
    //         false,
    //     )?;

    //     let client = HttpClient::new(tls_settings, cx.proxy())?;
    //     let healthcheck = healthcheck(
    //         service.clone(),
    //         client.clone(),
    //         self.default_api_key.clone(),
    //     )
    //     .boxed();
    //     // let sink = PartitionHttpSink::new(
    //     //     service,
    //     //     PartitionBuffer::new(batch),
    //     //     request_settings,
    //     //     timeout,
    //     //     client,
    //     //     cx.acker(),
    //     // )
    //     // .
    //     // let sink = VectorSink::Sink(Box::new(sink));
    //     let log_api = LogApi::new()
    //         .batch_timeout(Duration::from_secs(15))
    //         .batch_timeout(timeout)
    //         .bytes_stored_limit(bytesize::mib(5_u32))
    //         .compression(self.compression.unwrap_or_default())
    //         .datadog_uri(self.get_uri())
    //         .default_api_key(self.default_api_key.clone().into_boxed_str())
    //         .encoding(self.encoding.clone())
    //         .http_client(client)
    //         .log_schema(vector_core::config::log_schema())
    //         .build()?;
    //     //.sink_map_err(|error| error!(message = "Fatal datadog_logs text sink error.", %error));
    //     let sink = VectorSink::Stream(Box::new(log_api));

    //     Ok((sink, healthcheck))
    // }
}

// pub type Healthcheck = BoxFuture<'static, crate::Result<()>>;

async fn nop_healthcheck() -> crate::Result<()> {
    Ok(())
}

#[async_trait::async_trait]
#[typetag::serde(name = "datadog_logs")]
impl SinkConfig for DatadogLogsConfig {
    async fn build(&self, cx: SinkContext) -> crate::Result<(VectorSink, Healthcheck)> {
        // let batch_settings = self.batch_settings()?;
        // let service = service::Service::builder()
        //     .encoding(self.encoding.clone())
        //     .compression(self.compression.unwrap_or_default())
        //     .uri(self.get_uri())
        //     .default_api_key(Arc::from(self.default_api_key.clone()))
        //     .log_schema(vector_core::config::log_schema())
        //     .build();
        // self.build_sink(
        //     cx,
        //     service,
        //     JsonArrayBuffer::new(batch_settings.size),
        //     batch_settings.timeout,
        // )

        let tls_settings = MaybeTlsSettings::from_config(
            &Some(self.tls.clone().unwrap_or_else(TlsConfig::enabled)),
            false,
        )?;

        let client = HttpClient::new(tls_settings, cx.proxy())?;
        // let healthcheck = healthcheck(
        //     service.clone(),
        //     client.clone(),
        //     self.default_api_key.clone(),
        // )
        // .boxed();
        let healthcheck = nop_healthcheck().boxed();

        let log_api = LogApi::new()
            // .batch_timeout(batch_settings.timeout)
            // .bytes_stored_limit(batch_settings.size.bytes as u64)
            .bytes_stored_limit(bytesize::mib(5_u32))
            .compression(self.compression.unwrap_or_default())
            .datadog_uri(self.get_uri())
            .default_api_key(self.default_api_key.clone().into_boxed_str())
            .encoding(self.encoding.clone())
            .http_client(client)
            .log_schema(vector_core::config::log_schema())
            .build()?;
        let sink = VectorSink::Stream(Box::new(log_api));

        Ok((sink, healthcheck))
    }

    fn input_type(&self) -> DataType {
        DataType::Log
    }

    fn sink_type(&self) -> &'static str {
        "datadog_logs"
    }
}

#[cfg(test)]
mod test {
    use crate::sinks::datadog::logs::DatadogLogsConfig;

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<DatadogLogsConfig>();
    }
}
