use crate::{
    config::{log_schema, DataType, SourceConfig, SourceContext, SourceDescription},
    event::{Event, LookupBuf, Value},
    internal_events::{KafkaEventFailed, KafkaEventReceived, KafkaOffsetUpdateFailed},
    kafka::KafkaAuthConfig,
    shutdown::ShutdownSignal,
    Pipeline,
};
use bytes::Bytes;
use chrono::{TimeZone, Utc};
use futures::{SinkExt, StreamExt};
use rdkafka::{
    config::ClientConfig,
    consumer::{Consumer, StreamConsumer},
    message::Message,
};
use serde::{Deserialize, Serialize};
use snafu::{ResultExt, Snafu};
use std::{collections::HashMap, sync::Arc};

#[derive(Debug, Snafu)]
enum BuildError {
    #[snafu(display("Could not create Kafka consumer: {}", source))]
    KafkaCreateError { source: rdkafka::error::KafkaError },
    #[snafu(display("Could not subscribe to Kafka topics: {}", source))]
    KafkaSubscribeError { source: rdkafka::error::KafkaError },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KafkaSourceConfig {
    bootstrap_servers: String,
    topics: Vec<String>,
    group_id: String,
    #[serde(default = "default_auto_offset_reset")]
    auto_offset_reset: String,
    #[serde(default = "default_session_timeout_ms")]
    session_timeout_ms: u64,
    #[serde(default = "default_socket_timeout_ms")]
    socket_timeout_ms: u64,
    #[serde(default = "default_fetch_wait_max_ms")]
    fetch_wait_max_ms: u64,
    #[serde(default = "default_commit_interval_ms")]
    commit_interval_ms: u64,
    #[serde(default = "default_key_field")]
    key_field: LookupBuf,
    #[serde(default = "default_topic_key")]
    topic_key: LookupBuf,
    #[serde(default = "default_partition_key")]
    partition_key: LookupBuf,
    #[serde(default = "default_offset_key")]
    offset_key: LookupBuf,
    librdkafka_options: Option<HashMap<String, String>>,
    #[serde(flatten)]
    auth: KafkaAuthConfig,
}

impl Default for KafkaSourceConfig {
    fn default() -> Self {
        Self {
            bootstrap_servers: Default::default(),
            topics: Default::default(),
            group_id: Default::default(),
            auto_offset_reset: default_auto_offset_reset(),
            session_timeout_ms: default_session_timeout_ms(),
            socket_timeout_ms: default_socket_timeout_ms(),
            fetch_wait_max_ms: default_fetch_wait_max_ms(),
            commit_interval_ms: default_commit_interval_ms(),
            key_field: default_key_field(),
            topic_key: default_topic_key(),
            partition_key: default_partition_key(),
            offset_key: default_offset_key(),
            librdkafka_options: Default::default(),
            auth: Default::default(),
        }
    }
}

fn default_session_timeout_ms() -> u64 {
    10000 // default in librdkafka
}

fn default_socket_timeout_ms() -> u64 {
    60000 // default in librdkafka
}

fn default_fetch_wait_max_ms() -> u64 {
    100 // default in librdkafka
}

fn default_commit_interval_ms() -> u64 {
    5000 // default in librdkafka
}

fn default_auto_offset_reset() -> String {
    "largest".into() // default in librdkafka
}

fn default_key_field() -> LookupBuf {
    LookupBuf::from("message_key")
}

fn default_topic_key() -> LookupBuf {
    LookupBuf::from("topic")
}

fn default_partition_key() -> LookupBuf {
    LookupBuf::from("partition")
}

fn default_offset_key() -> LookupBuf {
    LookupBuf::from("offset")
}

inventory::submit! {
    SourceDescription::new::<KafkaSourceConfig>("kafka")
}

impl_generate_config_from_default!(KafkaSourceConfig);

#[async_trait::async_trait]
#[typetag::serde(name = "kafka")]
impl SourceConfig for KafkaSourceConfig {
    async fn build(&self, cx: SourceContext) -> crate::Result<super::Source> {
        kafka_source(self, cx.shutdown, cx.out)
    }

    fn output_type(&self) -> DataType {
        DataType::Log
    }

    fn source_type(&self) -> &'static str {
        "kafka"
    }
}

fn kafka_source(
    config: &KafkaSourceConfig,
    shutdown: ShutdownSignal,
    out: Pipeline,
) -> crate::Result<super::Source> {
    let key_field = config.key_field.clone();
    let topic_key = config.topic_key.clone();
    let partition_key = config.partition_key.clone();
    let offset_key = config.offset_key.clone();
    let consumer = Arc::new(create_consumer(config)?);

    Ok(Box::pin(async move {
        let shutdown = shutdown;

        Arc::clone(&consumer)
            .stream()
            .take_until(shutdown)
            .then(move |message| {
                let key_field = key_field.clone();
                let topic_key = topic_key.clone();
                let partition_key = partition_key.clone();
                let offset_key = offset_key.clone();
                let consumer = Arc::clone(&consumer);

                async move {
                    match message {
                        Err(error) => {
                            emit!(KafkaEventFailed { error });
                            Err(())
                        }
                        Ok(msg) => {
                            emit!(KafkaEventReceived {
                                byte_size: msg.payload_len()
                            });

                            let payload = match msg.payload() {
                                None => return Err(()), // skip messages with empty payload
                                Some(payload) => payload,
                            };
                            let mut event = Event::new_empty_log();
                            let log = event.as_mut_log();

                            log.insert(
                                log_schema().message_key().clone(),
                                Value::from(Bytes::from(payload.to_owned())),
                            );

                            // Extract timestamp from kafka message
                            let timestamp = msg
                                .timestamp()
                                .to_millis()
                                .and_then(|millis| Utc.timestamp_millis_opt(millis).latest())
                                .unwrap_or_else(Utc::now);
                            log.insert(log_schema().timestamp_key().clone(), timestamp);

                            // Add source type
                            log.insert(
                                log_schema().source_type_key().clone(),
                                Bytes::from("kafka"),
                            );

                            let msg_key = msg
                                .key()
                                .map(|key| Value::from(String::from_utf8_lossy(key).to_string()))
                                .unwrap_or(Value::Null);
                            log.insert(key_field, msg_key);

                            log.insert(topic_key, Value::from(msg.topic().to_string()));

                            log.insert(partition_key, Value::from(msg.partition()));

                            log.insert(offset_key, Value::from(msg.offset()));

                            consumer.store_offset(&msg).map_err(|error| {
                                emit!(KafkaOffsetUpdateFailed { error });
                            })?;

                            Ok(event)
                        }
                    }
                }
            })
            // Try `forward` after removing old futures.
            // Error: implementation of `futures_core::stream::Stream` is not general enough
            // .forward(
            //     out.sink_compat()
            //         .sink_map_err(|error| error!(message = "Error sending to sink.", %error)),
            // )
            .for_each(|item| {
                let mut out = out.clone();
                async move {
                    if let Ok(item) = item {
                        if let Err(error) = out.send(item).await {
                            error!(message = "Error sending to sink.", %error);
                        }
                    }
                }
            })
            .await;
        Ok(())
    }))
}

fn create_consumer(config: &KafkaSourceConfig) -> crate::Result<StreamConsumer> {
    let mut client_config = ClientConfig::new();
    client_config
        .set("group.id", &config.group_id)
        .set("bootstrap.servers", &config.bootstrap_servers)
        .set("auto.offset.reset", &config.auto_offset_reset)
        .set("session.timeout.ms", &config.session_timeout_ms.to_string())
        .set("socket.timeout.ms", &config.socket_timeout_ms.to_string())
        .set("fetch.wait.max.ms", &config.fetch_wait_max_ms.to_string())
        .set("enable.partition.eof", "false")
        .set("enable.auto.commit", "true")
        .set(
            "auto.commit.interval.ms",
            &config.commit_interval_ms.to_string(),
        )
        .set("enable.auto.offset.store", "false")
        .set("client.id", "vector");

    config.auth.apply(&mut client_config)?;

    if let Some(librdkafka_options) = &config.librdkafka_options {
        for (key, value) in librdkafka_options {
            client_config.set(key.as_str(), value.as_str());
        }
    }

    let consumer: StreamConsumer = client_config.create().context(KafkaCreateError)?;
    let topics: Vec<&str> = config.topics.iter().map(|s| s.as_str()).collect();
    consumer.subscribe(&topics).context(KafkaSubscribeError)?;

    Ok(consumer)
}

#[cfg(test)]
mod test {
    use super::{kafka_source, KafkaSourceConfig};
    use crate::{event::LookupBuf, shutdown::ShutdownSignal, Pipeline};

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<KafkaSourceConfig>();
    }

    fn make_config() -> KafkaSourceConfig {
        KafkaSourceConfig {
            bootstrap_servers: "localhost:9092".to_string(),
            topics: vec!["my-topic".to_string()],
            group_id: "group-id".to_string(),
            auto_offset_reset: "earliest".to_string(),
            session_timeout_ms: 10000,
            commit_interval_ms: 5000,
            key_field: LookupBuf::from("message_key"),
            topic_key: LookupBuf::from("topic"),
            partition_key: LookupBuf::from("partition"),
            offset_key: LookupBuf::from("offset"),
            socket_timeout_ms: 60000,
            fetch_wait_max_ms: 100,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn kafka_source_create_ok() {
        let config = make_config();
        assert!(kafka_source(&config, ShutdownSignal::noop(), Pipeline::new_test().0).is_ok());
    }

    #[tokio::test]
    async fn kafka_source_create_incorrect_auto_offset_reset() {
        let config = KafkaSourceConfig {
            auto_offset_reset: "incorrect-auto-offset-reset".to_string(),
            ..make_config()
        };
        assert!(kafka_source(&config, ShutdownSignal::noop(), Pipeline::new_test().0).is_err());
    }
}

#[cfg(feature = "kafka-integration-tests")]
#[cfg(test)]
mod integration_test {
    use super::*;
    use crate::{
        event::Lookup,
        shutdown::ShutdownSignal,
        test_util::{collect_n, random_string},
        Pipeline,
    };
    use chrono::{SubsecRound, Utc};
    use rdkafka::{
        config::ClientConfig,
        producer::{FutureProducer, FutureRecord},
        util::Timeout,
    };

    const BOOTSTRAP_SERVER: &str = "localhost:9091";

    async fn send_event(topic: String, key: &str, text: &str, timestamp: i64) {
        let producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", BOOTSTRAP_SERVER)
            .set("produce.offset.report", "true")
            .set("message.timeout.ms", "5000")
            .create()
            .expect("Producer creation error");

        let record = FutureRecord::to(&topic)
            .payload(text)
            .key(key)
            .timestamp(timestamp);

        if let Err(error) = producer.send(record, Timeout::Never).await {
            panic!("Cannot send event to Kafka: {:?}", error);
        }
    }

    #[tokio::test]
    async fn kafka_source_consume_event() {
        let topic = format!("test-topic-{}", random_string(10));
        println!("Test topic name: {}", topic);
        let group_id = format!("test-group-{}", random_string(10));
        let now = Utc::now();

        let config = KafkaSourceConfig {
            bootstrap_servers: BOOTSTRAP_SERVER.into(),
            topics: vec![topic.clone()],
            group_id: group_id.clone(),
            auto_offset_reset: "beginning".into(),
            session_timeout_ms: 6000,
            commit_interval_ms: 5000,
            key_field: LookupBuf::from("message_key"),
            topic_key: LookupBuf::from("topic"),
            partition_key: LookupBuf::from("partition"),
            offset_key: LookupBuf::from("offset"),
            socket_timeout_ms: 60000,
            fetch_wait_max_ms: 100,
            ..Default::default()
        };

        println!("Sending event...");
        send_event(
            topic.clone(),
            "my key",
            "my message",
            now.timestamp_millis(),
        )
        .await;

        println!("Receiving event...");
        let (tx, rx) = Pipeline::new_test();
        tokio::spawn(kafka_source(&config, ShutdownSignal::noop(), tx).unwrap());
        let events = collect_n(rx, 1).await;

        assert_eq!(
            events[0].as_log()[log_schema().message_key()],
            "my message".into()
        );
        assert_eq!(
            events[0].as_log()[Lookup::from("message_key")],
            "my key".into()
        );
        assert_eq!(
            events[0].as_log()[log_schema().source_type_key()],
            "kafka".into()
        );
        assert_eq!(
            events[0].as_log()[log_schema().timestamp_key()],
            now.trunc_subsecs(3).into()
        );
        assert_eq!(events[0].as_log()["topic"], topic.into());
        assert!(events[0].as_log().contains("partition"));
        assert!(events[0].as_log().contains("offset"));
    }
}
