use std::sync::Arc;
use std::u64;
use std::usize;

use super::super::{
    Datastore, EdgeDirection, EdgePropertyQuery, EdgeQuery, Transaction, VertexPropertyQuery, VertexQuery,
};
use super::managers::*;
use crate::errors::Result;
use crate::models;
use crate::models::*;
use crate::util::{next_uuid, remove_nones_from_iterator};

use chrono::offset::Utc;
use serde_json::Value as JsonValue;
use sled::{Db, Batch, Config};
use uuid::Uuid;

/// A datastore that is backed by Sled.
pub struct SledDatastore {
    pub(crate) db: Arc<Db>,
}

impl<'ds> SledDatastore {
    /// Creates a new Sled datastore.
    ///
    /// # Arguments
    /// * `path` - The file path to the Sled database.
    pub fn new(path: &str) -> Result<SledDatastore> {
        Ok(SledDatastore {
            db: Arc::new(sled::open(path)?),
        })
    }

    /// Creates a new Sled datastore.
    ///
    /// # Arguments
    /// * `path` - The file path to the Sled database.
    /// * `config` - Creates a new sled config with a given config.
    pub fn new_with_config(path: &str, config: SledConfig) -> Result<SledDatastore> {
        let mut sled_config = Config::default().path(path);

        if config.use_compression {
            sled_config = sled_config.use_compression(true);
        }

        if let Some(compression_factor) = config.compression_factor {
            sled_config = sled_config.compression_factor(compression_factor);
        }

        Ok(SledDatastore {
            db: Arc::new(sled_config.open()?),
        })
    }
}

impl Datastore for SledDatastore {
    type Trans = SledTransaction;

    fn transaction(&self) -> Result<Self::Trans> {
        SledTransaction::new(self.db.clone())
    }

    fn bulk_insert<I>(&self, items: I) -> Result<()>
    where
        I: Iterator<Item = models::BulkInsertItem>,
    {
        let vertex_manager = VertexManager::new(&self.db);
        let edge_manager = EdgeManager::new(&self.db);
        let vertex_property_manager = VertexPropertyManager::new(&self.db);
        let edge_property_manager = EdgePropertyManager::new(&self.db);
        let mut batch = Batch::default();

        for item in items {
            match item {
                models::BulkInsertItem::Vertex(ref vertex) => {
                    vertex_manager.create(vertex)?;
                }
                models::BulkInsertItem::Edge(ref key) => {
                    edge_manager.set(&mut batch, key.outbound_id, &key.t, key.inbound_id, Utc::now())?;
                }
                models::BulkInsertItem::VertexProperty(id, ref name, ref value) => {
                    vertex_property_manager.set(id, name, value)?;
                }
                models::BulkInsertItem::EdgeProperty(ref key, ref name, ref value) => {
                    edge_property_manager.set(key.outbound_id, &key.t, key.inbound_id, name, value)?;
                }
            }
        }

        self.db.apply_batch(batch)?;
        self.db.flush()?;
        Ok(())
    }
}

/// A transaction that is backed by Sled.
pub struct SledTransaction {
    db: Arc<Db>,
}

impl SledTransaction {
    fn new(db: Arc<Db>) -> Result<Self> {
        Ok(SledTransaction { db })
    }

    fn vertex_query_to_iterator<'iter, 'trans: 'iter>(
        &'trans self,
        q: VertexQuery,
    ) -> Result<Box<dyn Iterator<Item = Result<VertexItem>> + 'iter>> {
        match q {
            VertexQuery::Range(q) => {
                let vertex_manager = VertexManager::new(&self.db);

                let next_uuid = match q.start_id {
                    Some(start_id) => {
                        match next_uuid(start_id) {
                            Ok(next_uuid) => next_uuid,
                            // If we get an error back, it's because
                            // `start_id` is the maximum possible value. We
                            // know that no vertices exist whose ID is greater
                            // than the maximum possible value, so just return
                            // an empty list.
                            Err(_) => return Ok(Box::new(vec![].into_iter())),
                        }
                    }
                    None => Uuid::default(),
                };

                let mut iter: Box<dyn Iterator<Item = Result<VertexItem>>> =
                    Box::new(vertex_manager.iterate_for_range(next_uuid));

                if let Some(ref t) = q.t {
                    iter = Box::new(iter.filter(move |item| match item {
                        Ok((_, v)) => v == t,
                        Err(_) => true,
                    }));
                }

                let results: Vec<Result<VertexItem>> = iter.take(q.limit as usize).collect();
                Ok(Box::new(results.into_iter()))
            }
            VertexQuery::Specific(q) => {
                let vertex_manager = VertexManager::new(&self.db);

                let iter = q.ids.into_iter().map(move |id| match vertex_manager.get(id)? {
                    Some(value) => Ok(Some((id, value))),
                    None => Ok(None),
                });

                Ok(Box::new(remove_nones_from_iterator(iter)))
            }
            VertexQuery::Pipe(q) => {
                let vertex_manager = VertexManager::new(&self.db);
                let edge_iterator = self.edge_query_to_iterator(*q.inner)?;
                let direction = q.direction;

                let iter = edge_iterator.map(move |item| {
                    let (outbound_id, _, _, inbound_id) = item?;

                    let id = match direction {
                        EdgeDirection::Outbound => outbound_id,
                        EdgeDirection::Inbound => inbound_id,
                    };

                    match vertex_manager.get(id)? {
                        Some(value) => Ok(Some((id, value))),
                        None => Ok(None),
                    }
                });

                let mut iter: Box<dyn Iterator<Item = Result<VertexItem>>> = Box::new(remove_nones_from_iterator(iter));

                if let Some(ref t) = q.t {
                    iter = Box::new(iter.filter(move |item| match item {
                        Ok((_, v)) => v == t,
                        Err(_) => true,
                    }));
                }

                let results: Vec<Result<VertexItem>> = iter.take(q.limit as usize).collect();
                Ok(Box::new(results.into_iter()))
            }
        }
    }

    fn edge_query_to_iterator<'iter, 'trans: 'iter>(
        &'trans self,
        q: EdgeQuery,
    ) -> Result<Box<dyn Iterator<Item = Result<EdgeRangeItem>> + 'iter>> {
        match q {
            EdgeQuery::Specific(q) => {
                let edge_manager = EdgeManager::new(&self.db);

                let edges = q.keys.into_iter().map(move |key| {
                    match edge_manager.get(key.outbound_id, &key.t, key.inbound_id)? {
                        Some(update_datetime) => {
                            Ok(Some((key.outbound_id, key.t.clone(), update_datetime, key.inbound_id)))
                        }
                        None => Ok(None),
                    }
                });

                let iterator = remove_nones_from_iterator(edges);
                Ok(Box::new(iterator))
            }
            EdgeQuery::Pipe(q) => {
                let vertex_iterator = self.vertex_query_to_iterator(*q.inner)?;

                let edge_range_manager = match q.direction {
                    EdgeDirection::Outbound => EdgeRangeManager::new(&self.db),
                    EdgeDirection::Inbound => EdgeRangeManager::new_reversed(&self.db),
                };

                // Ideally we'd use iterators all the way down, but things
                // start breaking apart due to conditional expressions not
                // returning the same type signature, issues with `Result`s
                // and some of the iterators, etc. So at this point, we'll
                // just resort to building a vector.
                let mut edges: Vec<Result<EdgeRangeItem>> = Vec::new();

                for item in vertex_iterator {
                    let (id, _) = item?;
                    let edge_iterator = edge_range_manager.iterate_for_range(id, q.t.as_ref(), q.high);

                    for item in edge_iterator {
                        match item {
                            Ok((
                                edge_range_first_id,
                                edge_range_t,
                                edge_range_update_datetime,
                                edge_range_second_id,
                            )) => {
                                if let Some(low) = q.low {
                                    if edge_range_update_datetime < low {
                                        break;
                                    }
                                }

                                edges.push(match q.direction {
                                    EdgeDirection::Outbound => Ok((
                                        edge_range_first_id,
                                        edge_range_t,
                                        edge_range_update_datetime,
                                        edge_range_second_id,
                                    )),
                                    EdgeDirection::Inbound => Ok((
                                        edge_range_second_id,
                                        edge_range_t,
                                        edge_range_update_datetime,
                                        edge_range_first_id,
                                    )),
                                })
                            }
                            Err(_) => edges.push(item),
                        }

                        if edges.len() == q.limit as usize {
                            break;
                        }
                    }
                }

                Ok(Box::new(edges.into_iter()))
            }
        }
    }
}

impl Transaction for SledTransaction {
    fn create_vertex(&self, vertex: &models::Vertex) -> Result<bool> {
        let vertex_manager = VertexManager::new(&self.db);

        if vertex_manager.exists(vertex.id)? {
            Ok(false)
        } else {
            vertex_manager.create(vertex)?;
            Ok(true)
        }
    }

    fn get_vertices<Q: Into<models::VertexQuery>>(&self, q: Q) -> Result<Vec<models::Vertex>> {
        let iterator = self.vertex_query_to_iterator(q.into())?;

        let mapped = iterator.map(move |item| {
            let (id, t) = item?;
            let vertex = models::Vertex::with_id(id, t);
            Ok(vertex)
        });

        mapped.collect()
    }

    fn delete_vertices<Q: Into<models::VertexQuery>>(&self, q: Q) -> Result<()> {
        let iterator = self.vertex_query_to_iterator(q.into())?;
        let vertex_manager = VertexManager::new(&self.db);
        let mut batch = Batch::default();

        for item in iterator {
            let (id, _) = item?;
            vertex_manager.delete(&mut batch, id)?;
        }

        self.db.apply_batch(batch)?;
        Ok(())
    }

    fn get_vertex_count(&self) -> Result<u64> {
        let vertex_manager = VertexManager::new(&self.db);
        let iterator = vertex_manager.iterate_for_range(Uuid::default());
        Ok(iterator.count() as u64)
    }

    fn create_edge(&self, key: &models::EdgeKey) -> Result<bool> {
        let vertex_manager = VertexManager::new(&self.db);

        if !vertex_manager.exists(key.outbound_id)? || !vertex_manager.exists(key.inbound_id)? {
            Ok(false)
        } else {
            let mut batch = Batch::default();
            let edge_manager = EdgeManager::new(&self.db);
            edge_manager.set(&mut batch, key.outbound_id, &key.t, key.inbound_id, Utc::now())?;
            self.db.apply_batch(batch)?;
            Ok(true)
        }
    }

    fn get_edges<Q: Into<models::EdgeQuery>>(&self, q: Q) -> Result<Vec<models::Edge>> {
        let iterator = self.edge_query_to_iterator(q.into())?;

        let mapped = iterator.map(move |item: Result<EdgeRangeItem>| {
            let (outbound_id, t, update_datetime, inbound_id) = item?;
            let key = models::EdgeKey::new(outbound_id, t, inbound_id);
            let edge = models::Edge::new(key, update_datetime);
            Ok(edge)
        });

        mapped.collect()
    }

    fn delete_edges<Q: Into<models::EdgeQuery>>(&self, q: Q) -> Result<()> {
        let edge_manager = EdgeManager::new(&self.db);
        let vertex_manager = VertexManager::new(&self.db);
        let iterator = self.edge_query_to_iterator(q.into())?;
        let mut batch = Batch::default();

        for item in iterator {
            let (outbound_id, t, update_datetime, inbound_id) = item?;

            if vertex_manager.get(outbound_id)?.is_some() {
                edge_manager.delete(&mut batch, outbound_id, &t, inbound_id, update_datetime)?;
            };
        }

        self.db.apply_batch(batch)?;
        Ok(())
    }

    fn get_edge_count(&self, id: Uuid, t: Option<&models::Type>, direction: models::EdgeDirection) -> Result<u64> {
        let edge_range_manager = match direction {
            EdgeDirection::Outbound => EdgeRangeManager::new(&self.db),
            EdgeDirection::Inbound => EdgeRangeManager::new_reversed(&self.db),
        };

        let iter = edge_range_manager.iterate_for_range(id, t, None);
        let count = iter.count();

        Ok(count as u64)
    }

    fn get_vertex_properties(&self, q: VertexPropertyQuery) -> Result<Vec<models::VertexProperty>> {
        let manager = VertexPropertyManager::new(&self.db);
        let mut properties = Vec::new();

        for item in self.vertex_query_to_iterator(q.inner)? {
            let (id, _) = item?;
            let value = manager.get(id, &q.name)?;

            if let Some(value) = value {
                properties.push(models::VertexProperty::new(id, value));
            }
        }

        Ok(properties)
    }

    fn get_all_vertex_properties<Q: Into<VertexQuery>>(&self, q: Q) -> Result<Vec<models::VertexProperties>> {
        let manager = VertexPropertyManager::new(&self.db);
        let iterator = self.vertex_query_to_iterator(q.into())?;

        let iter = iterator.map(move |item| {
            let (id, t) = item?;
            let vertex = models::Vertex::with_id(id, t);

            let it = manager.iterate_for_owner(id);
            let props: Result<Vec<_>> = it.map(|r| r).collect();
            let props_iter = props?.into_iter();
            let props = props_iter
                .map(|((_, name), value)| models::NamedProperty::new(name, value))
                .collect();

            Ok(models::VertexProperties::new(vertex, props))
        });

        iter.collect()
    }

    fn set_vertex_properties(&self, q: VertexPropertyQuery, value: &JsonValue) -> Result<()> {
        let manager = VertexPropertyManager::new(&self.db);

        for item in self.vertex_query_to_iterator(q.inner)? {
            let (id, _) = item?;
            manager.set(id, &q.name, value)?;
        }
        Ok(())
    }

    fn delete_vertex_properties(&self, q: VertexPropertyQuery) -> Result<()> {
        let manager = VertexPropertyManager::new(&self.db);
        let mut batch = Batch::default();

        for item in self.vertex_query_to_iterator(q.inner)? {
            let (id, _) = item?;
            manager.delete(&mut batch, id, &q.name);
        }

        self.db.apply_batch(batch)?;
        Ok(())
    }

    fn get_edge_properties(&self, q: EdgePropertyQuery) -> Result<Vec<models::EdgeProperty>> {
        let manager = EdgePropertyManager::new(&self.db);
        let mut properties = Vec::new();

        for item in self.edge_query_to_iterator(q.inner)? {
            let (outbound_id, t, _, inbound_id) = item?;
            let value = manager.get(outbound_id, &t, inbound_id, &q.name)?;

            if let Some(value) = value {
                let key = models::EdgeKey::new(outbound_id, t, inbound_id);
                properties.push(models::EdgeProperty::new(key, value));
            }
        }

        Ok(properties)
    }

    fn get_all_edge_properties<Q: Into<EdgeQuery>>(&self, q: Q) -> Result<Vec<EdgeProperties>> {
        let manager = EdgePropertyManager::new(&self.db);
        let iterator = self.edge_query_to_iterator(q.into())?;

        let iter = iterator.map(move |item| {
            let (out_id, t, time, in_id) = item?;
            let edge = Edge::new(EdgeKey::new(out_id, t.clone(), in_id), time);
            let it = manager.iterate_for_owner(out_id, &t, in_id);
            let props: Result<Vec<_>> = it.map(|r| r).collect();
            let props_iter = props?.into_iter();
            let props = props_iter
                .map(|((_, _, _, name), value)| NamedProperty::new(name, value))
                .collect();

            Ok(EdgeProperties::new(edge, props))
        });

        iter.collect()
    }

    fn set_edge_properties(&self, q: EdgePropertyQuery, value: &JsonValue) -> Result<()> {
        let manager = EdgePropertyManager::new(&self.db);

        for item in self.edge_query_to_iterator(q.inner)? {
            let (outbound_id, t, _, inbound_id) = item?;
            manager.set(outbound_id, &t, inbound_id, &q.name, value)?;
        }
        Ok(())
    }

    fn delete_edge_properties(&self, q: EdgePropertyQuery) -> Result<()> {
        let manager = EdgePropertyManager::new(&self.db);
        let mut batch = Batch::default();

        for item in self.edge_query_to_iterator(q.inner)? {
            let (outbound_id, t, _, inbound_id) = item?;
            manager.delete(&mut batch, outbound_id, &t, inbound_id, &q.name);
        }
        
        self.db.apply_batch(batch)?;
        Ok(())
    }
}
