use std::borrow::Borrow;
use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::Mutex;
use tonic::transport::{Certificate, Channel, ClientTlsConfig};
use tonic::{IntoRequest, Request};

use crate::authorize::{ApplicationCredentials, TokenManager, TLS_CERTS};
use crate::datastore::api;
use crate::datastore::api::datastore_client::DatastoreClient;
use crate::datastore::{
    Entity, Error, Filter, FromValue, IntoEntity, Key, KeyID, Order, Query, Value,
};

use super::api::aggregation_query::aggregation::{Count, Sum};
use super::api::transaction_options::{ReadOnly, ReadWrite};
use super::{CompositeFilter, IndexExcluded, Transaction};

/// The Datastore client, tied to a specific project.
#[derive(Clone)]
pub struct Client {
    pub(crate) project_name: String,
    pub(crate) service: DatastoreClient<Channel>,
    pub(crate) token_manager: Arc<Mutex<TokenManager>>,
    pub(crate) index_excluded: IndexExcluded,
}

/// Opciones para el modo de crear la trx
#[derive(Debug, Clone, PartialEq)]
pub enum TrxOption {
    /// modo solo lectura
    ReadOnly,
    /// modo de escritura y lectura
    ReadWrite,
    /// modo por defecto
    Default,
}

/// Optiones para el tipo se Agregación
#[derive(Debug, Clone, PartialEq)]
pub enum Aggregation {
    ///
    Count(String),
    ///
    Sum(String, String),
    ///
    Avg(String, String),
}

impl Client {
    pub(crate) const DOMAIN_NAME: &'static str = "datastore.googleapis.com";
    pub(crate) const ENDPOINT: &'static str = "https://datastore.googleapis.com";
    pub(crate) const SCOPES: [&'static str; 2] = [
        "https://www.googleapis.com/auth/cloud-platform",
        "https://www.googleapis.com/auth/datastore",
    ];

    pub(crate) async fn construct_request<T: IntoRequest<T>>(
        &mut self,
        request: T,
    ) -> Result<Request<T>, Error> {
        let mut request = request.into_request();
        let token = self.token_manager.lock().await.token().await?;
        let metadata = request.metadata_mut();
        metadata.insert("authorization", token.parse().unwrap());
        Ok(request)
    }

    /// Creates a new client for the specified project.
    ///
    /// Credentials are looked up in the `GOOGLE_APPLICATION_CREDENTIALS` environment variable.
    pub async fn new(project_name: impl Into<String>) -> Result<Client, Error> {
        let path = env::var("GOOGLE_APPLICATION_CREDENTIALS")?;
        let path = Path::new(&path);
        let file = File::open(path)?;
        let creds = json::from_reader(file)?;

        Client::from_credentials(project_name, creds).await
    }

    /// Creates a new client for the specified project with custom credentials.
    pub async fn from_credentials(
        project_name: impl Into<String>,
        creds: ApplicationCredentials,
    ) -> Result<Client, Error> {
        let tls_config = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(TLS_CERTS))
            .domain_name(Client::DOMAIN_NAME);

        let channel =
            Channel::from_static(Client::ENDPOINT).tls_config(tls_config)?.connect().await?;

        Ok(Client {
            project_name: project_name.into(),
            service: DatastoreClient::new(channel),
            token_manager: Arc::new(Mutex::new(TokenManager::new(creds, Client::SCOPES.as_ref()))),
            index_excluded: IndexExcluded::new()?,
        })
    }

    /// Create a new transaction
    ///     - option_mode: Option for the transaction
    ///     - trx_id: Clave de la transacción anterior y que por algún motivo fallo y se ejecuto el rollback
    pub async fn new_transaction(
        &mut self,
        option_mode: TrxOption,
        trx_id: Option<Vec<u8>>,
    ) -> Result<Transaction, Error> {
        let trx_option = match option_mode {
            TrxOption::ReadOnly => Some(api::TransactionOptions {
                mode: Some(api::transaction_options::Mode::ReadOnly(ReadOnly { read_time: None })),
            }),
            TrxOption::ReadWrite => match trx_id {
                Some(trx) => Some(api::TransactionOptions {
                    mode: Some(api::transaction_options::Mode::ReadWrite(ReadWrite {
                        previous_transaction: trx,
                    })),
                }),
                None => None,
            },
            TrxOption::Default => None,
        };

        let request = api::BeginTransactionRequest {
            database_id: "".to_string(),
            project_id: self.project_name.clone(),
            transaction_options: trx_option,
        };

        let request = self.construct_request(request).await?;
        let response = self.service.begin_transaction(request).await?;
        let response = response.into_inner();

        Ok(Transaction::new(self.to_owned(), response.transaction))
    }

    /// Reserve the ID of an entity before creating it
    /// We can use it for transactions with related entities
    pub async fn allocate_tx(&mut self, keys: Vec<Key>) -> Result<Vec<Key>, Error> {
        let ks = keys.iter().map(|key| convert_key(self.project_name.as_str(), key)).collect();

        let request = api::AllocateIdsRequest {
            database_id: "".to_string(),
            project_id: self.project_name.clone(),
            keys: ks,
        };

        let request = self.construct_request(request).await?;
        let response = self.service.allocate_ids(request).await?;

        let response = response.into_inner();
        let keys = response.keys.into_iter().map(|f| api::Key::into(f)).collect::<Vec<Key>>();

        Ok(keys)
    }

    /// Gets an entity from a key.
    pub async fn get<T, K>(&mut self, key: K) -> Result<Option<T>, Error>
    where
        K: Borrow<Key>,
        T: FromValue,
    {
        let results = self.get_all(Some(key.borrow())).await?;
        Ok(results.into_iter().next().map(T::from_value).transpose()?)
    }

    /// Gets multiple entities from multiple keys.
    pub async fn get_all<T, K, I>(&mut self, keys: I) -> Result<Vec<T>, Error>
    where
        I: IntoIterator<Item = K>,
        K: Borrow<Key>,
        T: FromValue,
    {
        Ok(self.get_all_run(keys, None).await?)
    }

    /// Gets multiple entities from multiple keys associated with a transaction
    pub(crate) async fn get_all_run<T, K, I>(
        &mut self,
        keys: I,
        tx_id: Option<Vec<u8>>,
    ) -> Result<Vec<T>, Error>
    where
        I: IntoIterator<Item = K>,
        K: Borrow<Key>,
        T: FromValue,
    {
        let og_keys: Vec<K> = keys.into_iter().collect();
        let mut keys: Vec<_> = og_keys
            .iter()
            .map(|key| convert_key(self.project_name.as_str(), key.borrow()))
            .collect();
        let mut found = HashMap::new();

        while !keys.is_empty() {
            let request = match tx_id.to_owned() {
                Some(tx) => api::LookupRequest {
                    keys,
                    database_id: "".to_string(),
                    project_id: self.project_name.clone(),
                    read_options: Some(api::ReadOptions {
                        consistency_type: Some(api::read_options::ConsistencyType::Transaction(tx)),
                    }),
                },
                None => api::LookupRequest {
                    keys,
                    database_id: "".to_string(),
                    project_id: self.project_name.clone(),
                    read_options: None,
                },
            };

            let request = self.construct_request(request).await?;
            let response = self.service.lookup(request).await?;

            let response = response.into_inner();
            found.extend(
                response
                    .found
                    .into_iter()
                    .map(|val| val.entity.unwrap())
                    .map(Entity::from)
                    .map(|entity| (entity.key, entity.properties)),
            );
            keys = response.deferred;
        }

        let values: Vec<T> = og_keys
            .into_iter()
            .flat_map(|key| found.remove(key.borrow()))
            .map(FromValue::from_value)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(values)
    }

    /// Inserts a new entity and returns its key.
    /// If the entity's key is incomplete, the returned key will be one generated by the store for this entity.
    pub async fn put(&mut self, entity: impl IntoEntity) -> Result<Option<Key>, Error> {
        let entity = entity.into_entity()?;
        let result = self.put_all(Some(entity)).await?;
        Ok(result.into_iter().next().flatten())
    }

    /// Inserts new entities and returns their keys.
    /// If an entity's key is incomplete, its returned key will be one generated by the store for this entity.
    pub async fn put_all<T, I>(&mut self, entities: I) -> Result<Vec<Option<Key>>, Error>
    where
        I: IntoIterator<Item = T>,
        T: IntoEntity,
    {
        let entities: Vec<Entity> =
            entities.into_iter().map(IntoEntity::into_entity).collect::<Result<_, _>>()?;

        let mutations = entities
            .into_iter()
            .map(|entity| {
                let is_incomplete = entity.key.is_new || entity.key.is_incomplete();
                let entity = convert_entity(
                    self.project_name.as_str(),
                    entity,
                    self.index_excluded.to_owned(),
                );
                api::Mutation {
                    operation: if is_incomplete {
                        Some(api::mutation::Operation::Insert(entity))
                    } else {
                        Some(api::mutation::Operation::Upsert(entity))
                    },
                    conflict_detection_strategy: None,
                }
            })
            .collect();

        let request = api::CommitRequest {
            mutations,
            mode: api::commit_request::Mode::NonTransactional as i32,
            transaction_selector: None,
            database_id: "".to_string(),
            project_id: self.project_name.clone(),
        };
        let request = self.construct_request(request).await?;
        let response = self.service.commit(request).await?;
        let response = response.into_inner();
        let keys =
            response.mutation_results.into_iter().map(|result| result.key.map(Key::from)).collect();

        Ok(keys)
    }

    /// Deletes an entity identified by a key.
    pub async fn delete(&mut self, key: impl Borrow<Key>) -> Result<(), Error> {
        self.delete_all(Some(key.borrow())).await
    }

    /// Deletes multiple entities identified by multiple keys.
    pub async fn delete_all<T, I>(&mut self, keys: I) -> Result<(), Error>
    where
        I: IntoIterator<Item = T>,
        T: Borrow<Key>,
    {
        let mutations = keys
            .into_iter()
            .map(|key| convert_key(self.project_name.as_str(), key.borrow()))
            .map(|key| api::Mutation {
                operation: Some(api::mutation::Operation::Delete(key)),
                conflict_detection_strategy: None,
            })
            .collect();

        let request = api::CommitRequest {
            mutations,
            mode: api::commit_request::Mode::NonTransactional as i32,
            transaction_selector: None,
            database_id: "".to_string(),
            project_id: self.project_name.clone(),
        };
        let request = self.construct_request(request).await?;
        self.service.commit(request).await?;

        Ok(())
    }

    /// Runs a (potentially) complex query againt Datastore and returns the results.
    pub async fn query(&mut self, query: Query) -> Result<(Vec<Entity>, Vec<u8>), Error> {
        Ok(self.query_run(query, None).await?)
    }

    /// Runs a (potentially) complex query againt Datastore and returns the results and associated with a transaction
    pub(crate) async fn query_run(
        &mut self,
        query: Query,
        tx_id: Option<Vec<u8>>,
    ) -> Result<(Vec<Entity>, Vec<u8>), Error> {
        let mut output = Vec::new();

        let mut cur_query = query.clone();

        let mut cursor = match query.cursor.to_owned() {
            Some(c) => c,
            None => Vec::new(),
        };

        loop {
            let api_query = convert_query(&self.project_name, cur_query.to_owned(), cursor);

            let request = api::RunQueryRequest {
                partition_id: Some(api::PartitionId {
                    database_id: "".to_string(),
                    project_id: self.project_name.clone(),
                    namespace_id: cur_query.namespace.unwrap_or_else(String::new),
                }),
                query_type: Some(api::run_query_request::QueryType::Query(api_query)),
                read_options: Some({
                    use api::read_options::{ConsistencyType, ReadConsistency};
                    api::ReadOptions {
                        consistency_type: Some(match tx_id.to_owned() {
                            Some(tx) => ConsistencyType::Transaction(tx),
                            None => ConsistencyType::ReadConsistency(if cur_query.eventual {
                                ReadConsistency::Eventual as i32
                            } else {
                                ReadConsistency::Strong as i32
                            }),
                        }),
                    }
                }),
                database_id: "".to_string(),
                project_id: self.project_name.clone(),
            };

            let request = self.construct_request(request).await?;
            let results = self.service.run_query(request).await?;
            let results = results.into_inner().batch.unwrap();

            output.extend(
                results.entity_results.into_iter().map(|el| Entity::from(el.entity.unwrap())),
            );

            if results.more_results
                != (api::query_result_batch::MoreResultsType::NotFinished as i32)
            {
                break Ok((output, results.end_cursor));
            }

            cur_query = query.clone();
            cursor = results.end_cursor;
        }
    }

    /// Runs a (potentially) complex query againt Datastore and returns the results.
    pub async fn aggregation_query(
        &mut self,
        aggregations: Vec<Aggregation>,
        query: Query,
    ) -> Result<Vec<Value>, Error> {
        Ok(self.aggregation_query_run(aggregations, query, None).await?)
    }

    /// Runs a (potentially) complex query againt Datastore and returns the results and associated with a transaction
    pub(crate) async fn aggregation_query_run(
        &mut self,
        aggregations: Vec<Aggregation>,
        query: Query,
        tx_id: Option<Vec<u8>>,
    ) -> Result<Vec<Value>, Error> {
        let cur_query = query.clone();

        let cursor = match query.cursor.to_owned() {
            Some(c) => c,
            None => Vec::new(),
        };

        let api_query = convert_query(&self.project_name, cur_query.to_owned(), cursor);

        let aggregations = aggregations
            .to_vec()
            .into_iter()
            .map(|aggr| match aggr {
                super::Aggregation::Count(alias) => {
                    let operator = api::aggregation_query::aggregation::Operator::Count(Count {
                        up_to: Some(1000),
                    });
                    api::aggregation_query::Aggregation { operator: Some(operator), alias }
                }
                super::Aggregation::Sum(alias, property) => {
                    let operator = api::aggregation_query::aggregation::Operator::Sum(Sum {
                        property: Some(api::PropertyReference { name: property }),
                    });
                    api::aggregation_query::Aggregation { operator: Some(operator), alias }
                }
                super::Aggregation::Avg(alias, property) => {
                    let operator = api::aggregation_query::aggregation::Operator::Avg(
                        api::aggregation_query::aggregation::Avg {
                            property: Some(api::PropertyReference { name: property }),
                        },
                    );
                    api::aggregation_query::Aggregation { operator: Some(operator), alias }
                }
            })
            .collect::<Vec<api::aggregation_query::Aggregation>>();

        let aggregation_query: api::AggregationQuery = api::AggregationQuery {
            aggregations,
            query_type: Some(api::aggregation_query::QueryType::NestedQuery(api_query.clone())),
        };

        let request = api::RunAggregationQueryRequest {
            partition_id: Some(api::PartitionId {
                database_id: "".to_string(),
                project_id: self.project_name.clone(),
                namespace_id: cur_query.namespace.unwrap_or_else(String::new),
            }),
            query_type: Some(api::run_aggregation_query_request::QueryType::AggregationQuery(
                aggregation_query,
            )),
            read_options: Some({
                use api::read_options::{ConsistencyType, ReadConsistency};
                api::ReadOptions {
                    consistency_type: Some(match tx_id.to_owned() {
                        Some(tx) => ConsistencyType::Transaction(tx),
                        None => ConsistencyType::ReadConsistency(if cur_query.eventual {
                            ReadConsistency::Eventual as i32
                        } else {
                            ReadConsistency::Strong as i32
                        }),
                    }),
                }
            }),
            database_id: "".to_string(),
            project_id: self.project_name.clone(),
        };
        let request = self.construct_request(request).await?;
        let results = self.service.run_aggregation_query(request).await?;
        let results = results.into_inner().batch.unwrap();

        Ok(results
            .aggregation_results
            .into_iter()
            .map(|el| {
                let properties = el
                    .aggregate_properties
                    .into_iter()
                    .map(|(k, v)| (k, Value::from(v.value_type.unwrap())))
                    .collect();
                Value::EntityValue(properties)
            })
            .collect::<Vec<Value>>())
    }
}

fn convert_query(project_name: &str, cur_query: Query, cursor: Vec<u8>) -> api::Query {
    let projection = cur_query
        .projections
        .into_iter()
        .map(|name| api::Projection { property: Some(api::PropertyReference { name }) })
        .collect();
    let filter = convert_filter(project_name, cur_query.filters, cur_query.composite_filter);
    let order = cur_query
        .ordering
        .into_iter()
        .map(|order| {
            use api::property_order::Direction;
            let (name, direction) = match order {
                Order::Asc(name) => (name, Direction::Ascending),
                Order::Desc(name) => (name, Direction::Descending),
            };
            api::PropertyOrder {
                property: Some(api::PropertyReference { name }),
                direction: direction as i32,
            }
        })
        .collect();
    api::Query {
        kind: vec![api::KindExpression { name: cur_query.kind }],
        projection,
        filter,
        order,
        offset: cur_query.offset,
        limit: cur_query.limit,
        start_cursor: cursor,
        end_cursor: Vec::new(),
        distinct_on: cur_query
            .distinct_on
            .into_iter()
            .map(|name| api::PropertyReference { name })
            .collect(),
    }
}

pub(crate) fn convert_key(project_name: &str, key: &Key) -> api::Key {
    api::Key {
        partition_id: Some(api::PartitionId {
            database_id: "".to_string(),
            project_id: String::from(project_name),
            namespace_id: key.get_namespace().map(String::from).unwrap_or_default(),
        }),
        path: {
            let mut key = Some(key);
            let mut path = Vec::new();
            while let Some(current) = key {
                path.push(api::key::PathElement {
                    kind: String::from(current.get_kind()),
                    id_type: match current.get_id() {
                        KeyID::Incomplete => None,
                        KeyID::IntID(id) => Some(api::key::path_element::IdType::Id(*id)),
                        KeyID::StringID(id) => {
                            Some(api::key::path_element::IdType::Name(id.clone()))
                        }
                    },
                });
                key = current.get_parent();
            }
            path.reverse();
            path
        },
    }
}

pub(crate) fn convert_entity(
    project_name: &str,
    entity: Entity,
    index_excluded: IndexExcluded,
) -> api::Entity {
    let key = convert_key(project_name, &entity.key);
    let properties = match entity.clone().properties {
        Value::EntityValue(properties) => properties,
        _ => panic!("unexpected non-entity datastore value"),
    };
    let properties = properties
        .into_iter()
        .map(|(k, v)| {
            let path_excluded = IndexExcluded::ckeck_value(
                index_excluded.to_owned(),
                entity.key.get_kind().to_owned(),
                k.to_owned(),
            );
            (
                k,
                convert_value(
                    project_name,
                    v,
                    path_excluded.to_vec(),
                    check_exclude_from_indexes(path_excluded),
                ),
            )
        })
        .collect();
    api::Entity { key: Some(key), properties }
}

pub(crate) fn convert_value(
    project_name: &str,
    value: Value,
    path_excluded: Vec<String>,
    index_excluded: bool,
) -> api::Value {
    api::Value {
        meaning: 0,
        exclude_from_indexes: match value.to_owned() {
            Value::OptionValue(val) => match val {
                Some(v) => match *v {
                    Value::ArrayValue(_) => false,
                    _ => index_excluded,
                },
                None => index_excluded,
            },
            Value::ArrayValue(_) => false,
            _ => index_excluded,
        },
        value_type: Some(convert_value_type(project_name, value, path_excluded, index_excluded)),
    }
}

fn convert_value_type(
    project_name: &str,
    value: Value,
    path_excluded: Vec<String>,
    index_excluded: bool,
) -> api::value::ValueType {
    match value {
        Value::OptionValue(val) => match val {
            Some(v) => convert_value_type(project_name, *v, path_excluded, index_excluded),
            None => api::value::ValueType::NullValue(0),
        },
        Value::BooleanValue(val) => api::value::ValueType::BooleanValue(val),
        Value::IntegerValue(val) => api::value::ValueType::IntegerValue(val),
        Value::DoubleValue(val) => api::value::ValueType::DoubleValue(val),
        Value::TimestampValue(val) => {
            api::value::ValueType::TimestampValue(prost_types::Timestamp {
                seconds: val.and_utc().timestamp(),
                nanos: val.and_utc().timestamp_subsec_nanos() as i32,
            })
        }
        Value::KeyValue(key) => api::value::ValueType::KeyValue(convert_key(project_name, &key)),
        Value::StringValue(val) => api::value::ValueType::StringValue(val),
        Value::BlobValue(val) => api::value::ValueType::BlobValue(val),
        Value::GeoPointValue(latitude, longitude) => {
            api::value::ValueType::GeoPointValue(api::LatLng { latitude, longitude })
        }
        Value::EntityValue(properties) => api::value::ValueType::EntityValue({
            api::Entity {
                key: None,
                properties: properties
                    .into_iter()
                    .map(|(k, v)| {
                        let new_list_excluded =
                            get_exclude_from_indexes(path_excluded.to_vec(), k.to_owned());
                        (
                            k.to_owned(),
                            convert_value(
                                project_name,
                                v,
                                new_list_excluded.to_vec(),
                                check_exclude_from_indexes(new_list_excluded.to_vec()),
                            ),
                        )
                    })
                    .collect(),
            }
        }),
        Value::ArrayValue(values) => api::value::ValueType::ArrayValue(api::ArrayValue {
            values: values
                .into_iter()
                .map(|value| {
                    convert_value(project_name, value, path_excluded.to_vec(), index_excluded)
                })
                .collect(),
        }),
    }
}

fn get_exclude_from_indexes(list_excluded: Vec<String>, property: String) -> Vec<String> {
    list_excluded
        .to_vec()
        .into_iter()
        .filter_map(|element| {
            element
                .split_once(".")
                .or(Some((element.as_str(), "")))
                .filter(|(first, _)| first.to_string() == property)
                .map(|(_, rest)| rest.to_string())
        })
        .collect::<Vec<String>>()
}

fn check_exclude_from_indexes(list_excluded: Vec<String>) -> bool {
    match list_excluded.len() == 1 {
        true => list_excluded.first().unwrap() == "",
        false => false,
    }
}

pub(crate) fn convert_filter(
    project_name: &str,
    filters: Vec<Filter>,
    composite_filter: CompositeFilter,
) -> Option<api::Filter> {
    use api::filter::FilterType;

    if !filters.is_empty() {
        let filters = filters
            .into_iter()
            .map(|filter| {
                use api::property_filter::Operator;
                let (name, op, value) = match filter {
                    Filter::Equal(name, value) => (name, Operator::Equal, value),
                    Filter::GreaterThan(name, value) => (name, Operator::GreaterThan, value),
                    Filter::LessThan(name, value) => (name, Operator::LessThan, value),
                    Filter::GreaterThanOrEqual(name, value) => {
                        (name, Operator::GreaterThanOrEqual, value)
                    }
                    Filter::LessThanOrEqual(name, value) => {
                        (name, Operator::LessThanOrEqual, value)
                    }
                    Filter::HasAncestor(value) => {
                        ("__key__".to_string(), Operator::HasAncestor, value)
                    }
                    Filter::In(name, value) => (name, Operator::In, value),
                    Filter::NotIn(name, value) => (name, Operator::NotIn, value),
                    Filter::NotEqual(name, value) => (name, Operator::NotEqual, value),
                };

                api::Filter {
                    filter_type: Some(FilterType::PropertyFilter(api::PropertyFilter {
                        op: op as i32,
                        property: Some(api::PropertyReference { name }),
                        value: Some(convert_value(project_name, value, vec![], false)),
                    })),
                }
            })
            .collect();

        Some(api::Filter {
            filter_type: Some(FilterType::CompositeFilter(api::CompositeFilter {
                op: match composite_filter {
                    CompositeFilter::And => api::composite_filter::Operator::And as i32,
                    CompositeFilter::Or => api::composite_filter::Operator::Or as i32,
                },
                filters,
            })),
        })
    } else {
        None
    }
}
