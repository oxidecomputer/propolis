use futures::future::{self, BoxFuture};
use std::any::Any;
use std::collections::{btree_set, hash_map, BTreeMap, BTreeSet, HashMap};
use std::io::{Error as IoError, ErrorKind};
use std::sync::{Arc, Mutex, MutexGuard};
use thiserror::Error;

use crate::migrate::Migrator;

/// Errors returned while registering or deregistering from [`Inventory`].
#[derive(Error, Debug, PartialEq)]
pub enum RegistrationError {
    #[error("Cannot re-register object")]
    AlreadyRegistered,

    #[error("Cannot register object with duplicate instance name")]
    InstanceNameNotUnique,

    #[error("Cannot find parent with ID: {0:?}")]
    MissingParent(EntityID),

    #[error("Cannot find entity with ID: {0:?}")]
    MissingEntity(EntityID),

    #[error("Cannot insert root into non-empty inventory")]
    NotEmpty,
}

impl From<RegistrationError> for IoError {
    fn from(e: RegistrationError) -> IoError {
        use RegistrationError::*;
        match e {
            AlreadyRegistered => {
                IoError::new(ErrorKind::AlreadyExists, "already registered")
            }
            InstanceNameNotUnique => IoError::new(
                ErrorKind::AlreadyExists,
                "duplicate entity instance",
            ),
            MissingParent(_) => {
                IoError::new(ErrorKind::NotFound, "missing parent")
            }
            MissingEntity(_) => {
                IoError::new(ErrorKind::NotFound, "missing entity")
            }
            NotEmpty => {
                IoError::new(ErrorKind::AlreadyExists, "non-empty inventory")
            }
        }
    }
}

// Derives an address-based ID from an object which implements Entity.
trait ObjectID {
    fn object_id(&self) -> usize;
}

impl<T: Entity + ?Sized> ObjectID for Arc<T> {
    fn object_id(&self) -> usize {
        Arc::as_ptr(self) as *const () as usize
    }
}

/// A collection of virtual devices.
///
/// Inventory contains a collection of entities, which form a device tree.
pub struct Inventory {
    inner: Mutex<InventoryInner>,
}

impl Inventory {
    /// Initializes a new empty inventory.
    pub(crate) fn new() -> Self {
        Inventory { inner: Mutex::new(InventoryInner::default()) }
    }

    /// Registers a new entity with the inventory.
    ///
    /// Returns an error if the object has already been registered.
    /// Returns an error if the parent is not registered.
    pub fn register<T: Entity>(
        &self,
        ent: &Arc<T>,
    ) -> Result<EntityID, RegistrationError> {
        let mut inv = self.inner.lock().unwrap();
        inv.register(ent, None)
    }

    /// Registers a new entity, bearing an instance name, with the inventory.
    ///
    /// Returns an error if the object has already been registered.
    pub fn register_instance<T: Entity>(
        &self,
        ent: &Arc<T>,
        instance: impl ToString,
    ) -> Result<EntityID, RegistrationError> {
        let mut inv = self.inner.lock().unwrap();
        inv.register(ent, Some(instance.to_string()))
    }

    /// Registers a new entity, bearing a specific parent, with the inventory.
    ///
    /// Returns an error if the object has already been registered.
    /// Returns an error if the parent is not registered.
    pub fn register_child(
        &self,
        reg: ChildRegister,
        parent_id: EntityID,
    ) -> Result<EntityID, RegistrationError> {
        let mut inv = self.inner.lock().unwrap();
        inv.register_inner(
            reg.ent,
            reg.ent_any,
            reg.instance_name,
            Some(parent_id),
        )
    }

    /// Access the concrete type of an entity by ID.
    ///
    /// Returns the entity if it exists and has the requested concrete type.
    pub fn get_concrete<T: Entity>(&self, id: EntityID) -> Option<Arc<T>> {
        let inv = self.inner.lock().unwrap();
        inv.get_concrete(id)
    }

    /// Access the concrete type of an entity by name.
    ///
    /// Returns the entity if it exists and has the requested concrete type.
    pub fn get_concrete_by_name<T: Entity>(
        &self,
        instance_name: &str,
    ) -> Option<Arc<T>> {
        let inv = self.inner.lock().unwrap();
        inv.get_concrete_by_name(instance_name)
    }

    /// Lookup an entity by its instance name.
    pub fn get_by_name(&self, instance_name: &str) -> Option<Arc<dyn Entity>> {
        let inv = self.inner.lock().unwrap();
        inv.get_by_name(instance_name).map(|rec| Arc::clone(rec.entity()))
    }

    /// Return a list of entity instance names
    pub fn get_names(&self) -> Vec<String> {
        let inv = self.inner.lock().unwrap();
        inv.get_names()
    }

    /// Removes an entity from the inventory.
    ///
    /// No-op if the entity does not exist.
    pub fn deregister(&self, id: EntityID) -> Result<(), RegistrationError> {
        let mut inv = self.inner.lock().unwrap();
        inv.deregister(id)
    }

    /// Returns true if the inventory is empty.
    pub fn is_empty(&self) -> bool {
        let inv = self.inner.lock().unwrap();
        inv.is_empty()
    }

    /// Looks up a record associated with `id`, and invokes `func` with the
    /// result.
    ///
    /// NOTE: `func` must not invoke any other methods on `&self`.
    pub fn for_record<F>(&self, id: EntityID, func: F)
    where
        F: FnOnce(Option<&Record>),
    {
        let inv = self.inner.lock().unwrap();
        func(inv.get(id))
    }

    /// Executes `func` for every record within the inventory.
    ///
    /// NOTE: `func` must not invoke any other methods on `&self`.
    pub fn for_each_node<E, F>(
        &self,
        order: Order,
        mut func: F,
    ) -> Result<(), E>
    where
        F: FnMut(EntityID, &Record) -> Result<(), E>,
    {
        let inv = self.inner.lock().unwrap();
        for (eid, record) in inv.iter(order) {
            func(eid, record)?;
        }
        Ok(())
    }

    pub fn lock(&self) -> Guard {
        Guard(self.inner.lock().unwrap())
    }

    pub(crate) fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.clear();
    }
}

pub struct Guard<'a>(MutexGuard<'a, InventoryInner>);
impl<'a> Guard<'a> {
    pub fn iter(&self, order: Order) -> Iter {
        self.0.iter(order)
    }
    pub fn print(&self) {
        self.0.print()
    }
}

#[derive(Default)]
struct InventoryInner {
    // Mapping of inventory-assigned ID to the entity's Record.
    entities: BTreeMap<EntityID, Record>,
    // Set of IDs for entities which have no parent
    roots: BTreeSet<EntityID>,
    // Mapping of entity address to inventory-assigned ID.
    reverse: HashMap<usize, EntityID>,
    // Mapping of an entity's instance name to the inventory-assigned ID.
    reverse_name: HashMap<String, EntityID>,
    next_id: u64,
}

impl InventoryInner {
    /// Registers a new entity (and its children) with the inventory.
    ///
    /// Returns an error if the object has already been registered.
    pub fn register<T: Entity>(
        &mut self,
        ent: &Arc<T>,
        instance_name: Option<String>,
    ) -> Result<EntityID, RegistrationError> {
        let any = Arc::clone(ent) as Arc<dyn Any + Send + Sync>;
        let dyn_ent = Arc::clone(ent) as Arc<dyn Entity>;
        let children_to_register = ent.child_register();

        let id = self.register_inner(dyn_ent, any, instance_name, None)?;

        if let Some(children) = children_to_register {
            for child in children {
                // Since the parent successfully registered, the children should
                // have no issues.
                self.register_inner(
                    child.ent,
                    child.ent_any,
                    child.instance_name,
                    Some(id),
                )
                .unwrap();
            }
        }
        Ok(id)
    }

    /// Internal interface to register new entity with the inventory.
    ///
    /// Returns an error if the object has already been registered.
    /// Returns an error if the parent is not registered.
    fn register_inner(
        &mut self,
        ent: Arc<dyn Entity>,
        any: Arc<dyn Any + Send + Sync + 'static>,
        instance_name: Option<String>,
        parent_id: Option<EntityID>,
    ) -> Result<EntityID, RegistrationError> {
        let id = self.next_id();
        let obj_id = ent.object_id();

        match self.reverse.entry(obj_id) {
            hash_map::Entry::Occupied(_) => {
                return Err(RegistrationError::AlreadyRegistered)
            }
            hash_map::Entry::Vacant(hme) => hme.insert(id),
        };

        let name = instance_name
            .map(|inst| format!("{}-{}", ent.type_name(), inst))
            .unwrap_or_else(|| ent.type_name().to_string());

        match self.reverse_name.entry(name.clone()) {
            hash_map::Entry::Occupied(_) => {
                // undo entry in reverse table before bailing
                self.reverse.remove(&obj_id).unwrap();
                return Err(RegistrationError::InstanceNameNotUnique);
            }
            hash_map::Entry::Vacant(hme) => hme.insert(id),
        };

        if let Some(pid) = &parent_id {
            if let Some(parent) = self.entities.get_mut(pid) {
                parent.children.insert(id);
            } else {
                // undo entry in reverse tables before bailing
                self.reverse.remove(&obj_id).unwrap();
                self.reverse_name.remove(&name).unwrap();
                return Err(RegistrationError::MissingParent(*pid));
            }
        } else {
            self.roots.insert(id);
        }

        let rec = Record::new(ent, any, name, parent_id);
        self.entities.insert(id, rec);

        Ok(id)
    }

    /// Access the concrete type of an entity by ID.
    ///
    /// Returns the entity if it exists and has the requested concrete type.
    fn get_concrete<T: Entity>(&self, id: EntityID) -> Option<Arc<T>> {
        self.entities.get(&id)?.concrete()
    }

    /// Access the concrete type of an entity by name.
    ///
    /// Returns the entity if it exists and has the requested concrete type.
    fn get_concrete_by_name<T: Entity>(
        &self,
        instance_name: &str,
    ) -> Option<Arc<T>> {
        let id = self.reverse_name.get(instance_name)?;
        self.entities.get(&id)?.concrete()
    }

    /// Access an entity's record by ID.
    fn get(&self, id: EntityID) -> Option<&Record> {
        self.entities.get(&id)
    }

    /// Access an entity's record by its instance name.
    fn get_by_name(&self, instance_name: &str) -> Option<&Record> {
        let id = self.reverse_name.get(instance_name)?;
        self.entities.get(id)
    }

    /// Return a list of entity instance names
    fn get_names(&self) -> Vec<String> {
        self.reverse_name.keys().cloned().collect()
    }

    /// Removes an entity (and all its children) from the inventory.
    ///
    /// No-op if the entity does not exist.
    fn deregister(&mut self, id: EntityID) -> Result<(), RegistrationError> {
        // Remove all children recursively.
        let children = {
            let record = self
                .entities
                .get_mut(&id)
                .ok_or(RegistrationError::MissingEntity(id))?;
            std::mem::take(&mut record.children)
        };
        for child in &children {
            self.deregister(*child)?;
        }

        // Remove the current entity.
        let record = self.entities.remove(&id).unwrap();
        let entptr = record.ent.object_id();
        self.reverse.remove(&entptr);
        self.reverse_name.remove(record.name());

        // If this entity exists in the parent, remove it.
        //
        // Note that the parent may not exist, or it may have already been
        // removed as a part of the recursive walk.
        if let Some(parent) = record.parent {
            let parent = self.entities.get_mut(&parent).unwrap();
            parent.children.remove(&id);
        } else {
            self.roots.remove(&id);
        }

        Ok(())
    }

    /// Returns true if the inventory is empty.
    fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    fn next_id(&mut self) -> EntityID {
        self.next_id += 1;
        EntityID { num: self.next_id }
    }

    /// Iterates through the inventory according to the specified order.
    fn iter(&self, order: Order) -> Iter<'_> {
        Iter::new(self, order)
    }

    fn clear(&mut self) {
        self.entities.clear();
        self.roots.clear();
        self.reverse.clear();
        self.reverse_name.clear();
    }

    fn print(&self) {
        let mut stack: Vec<EntityID> = Vec::new();
        for (id, rec) in self.iter(Order::Pre) {
            let rec_parent = rec.parent();
            while let Some(eid) = stack.pop() {
                if matches!(rec_parent, Some(id) if id == eid) {
                    stack.push(eid);
                    break;
                }
            }
            stack.push(id);

            let depth = stack.len() - 1;
            println!(
                "{}- {}: {}",
                "  ".repeat(depth),
                u64::from(id),
                rec.name()
            );
        }
    }
}

/// Describes the iteration order through the inventory.
pub enum Order {
    /// Children before parent nodes
    Post,
    /// Parent before any child nodes
    Pre,
}

struct IterNode<'a> {
    post_visit: Option<(EntityID, &'a Record)>,
    items: btree_set::Iter<'a, EntityID>,
}

pub struct Iter<'a> {
    inv: &'a InventoryInner,
    stack: Vec<IterNode<'a>>,
    order: Order,
}

impl<'a> Iter<'a> {
    fn new(inv: &'a InventoryInner, order: Order) -> Self {
        Self {
            stack: vec![IterNode { post_visit: None, items: inv.roots.iter() }],
            inv,
            order,
        }
    }
}

impl<'a> Iterator for Iter<'a> {
    type Item = (EntityID, &'a Record);

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(mut node) = self.stack.pop() {
            if let Some(eid) = node.items.next() {
                self.stack.push(node);

                let rec = self.inv.entities.get(eid).unwrap();
                if !rec.children.is_empty() {
                    if matches!(self.order, Order::Pre) {
                        // Visit the children of this node after it is emitted.
                        self.stack.push(IterNode {
                            post_visit: None,
                            items: rec.children.iter(),
                        });
                        return Some((*eid, rec));
                    } else {
                        // Proceed deeper into the tree, taking note of our
                        // parent so it can be processed on the way back out
                        self.stack.push(IterNode {
                            post_visit: Some((*eid, rec)),
                            items: rec.children.iter(),
                        });
                    }
                } else {
                    // Emit leaf entry
                    return Some((*eid, rec));
                }
            } else {
                // Done with all children for a node, perform the post-ordered
                // visit, if necessary.
                if let Some(post) = node.post_visit.take() {
                    debug_assert!(matches!(self.order, Order::Post));
                    return Some(post);
                }
            }
        }
        None
    }
}

/// A device node within the inventory.
pub struct Record {
    any: Arc<dyn Any + Send + Sync + 'static>,
    ent: Arc<dyn Entity>,
    parent: Option<EntityID>,
    children: BTreeSet<EntityID>,
    name: String,
}

impl Record {
    fn new(
        ent: Arc<dyn Entity>,
        any: Arc<dyn Any + Send + Sync + 'static>,
        name: String,
        parent: Option<EntityID>,
    ) -> Self {
        Record { any, ent, parent, children: BTreeSet::new(), name }
    }

    /// Returns the concrete type of a record, or None if the wrong type is
    /// specified.
    pub fn concrete<T: Entity>(&self) -> Option<Arc<T>> {
        self.any.clone().downcast::<T>().ok()
    }

    /// Returns the entity-based type of the record.
    pub fn entity(&self) -> &Arc<dyn Entity> {
        &self.ent
    }

    /// Returns the name of the record used during registration.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn parent(&self) -> Option<EntityID> {
        self.parent
    }
}

pub trait Entity: Send + Sync + 'static {
    /// Unique name for entities for a given type
    fn type_name(&self) -> &'static str;

    #[allow(unused_variables)]
    fn child_register(&self) -> Option<Vec<ChildRegister>> {
        None
    }

    /// Return a future indicating when the device has finished pausing.
    fn paused(&self) -> BoxFuture<'static, ()> {
        Box::pin(future::ready(()))
    }

    /// Called when an instance is about to begin running, just before any
    /// vCPU threads are started. After this returns, the callee entity should
    /// be ready to do work on the guest's behalf.
    ///
    /// Note that this is only called the first time an instance begins running.
    /// If it reboots, the entity will observe a paused -> reset -> resumed
    /// transition instead.
    fn start(&self) -> anyhow::Result<()> {
        Ok(())
    }

    /// Directs this entity to pause. A paused entity must stop producing work
    /// for other entities, but must accept (and hold onto) new work from other
    /// entities while in the paused state.
    ///
    /// The entity is not required to finish pausing inline. Instead, its
    /// implementation of [`Entity::paused`] should return a future that
    /// completes only when the entity is paused.
    fn pause(&self) {}

    /// Directs this entity to resume servicing the guest after pausing.
    ///
    /// N.B. It is legal to interpose a `reset` between a pause and resume.
    ///      If one occurs, the state driver ensures that the entire VM will be
    ///      reset and reinitialized before any entities are resumed.
    fn resume(&self) {}

    /// Directs this entity to reset itself to the state it would have on a cold
    /// start.
    ///
    /// N.B. The state driver ensures this is called only on paused entities.
    ///      It also ensures that the entire VM will be reset and reinitialized
    ///      before resuming any entities.
    fn reset(&self) {}

    /// Indicates that the entity's instance is stopping and will soon be
    /// discarded.
    ///
    /// N.B. The state driver ensures this is called only on paused entities.
    fn halt(&self) {}

    /// Return the Migrator object that will be used to export/import
    /// this device's state.
    ///
    /// By default, we return a simple impl that assumes the device
    /// has no state that needs to be exported/imported but still wants
    /// to opt into being migratable. For more complex cases, a device
    /// may implement the `Migrate` trait along with its export/import
    /// methods. A device which shouldn't be migrated should instead
    /// override this method and explicity return `Migrator::NonMigratable`.
    fn migrate(&'_ self) -> Migrator<'_> {
        Migrator::Empty
    }
}

impl std::fmt::Debug for dyn Entity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Entity ({})", self.type_name())
    }
}

pub struct ChildRegister {
    ent: Arc<dyn Entity>,
    ent_any: Arc<dyn Any + Send + Sync + 'static>,
    instance_name: Option<String>,
}
impl ChildRegister {
    pub fn new<T: Entity>(ent: &Arc<T>, instance_name: Option<String>) -> Self {
        Self {
            ent: Arc::clone(ent) as Arc<dyn Entity>,
            ent_any: Arc::clone(ent) as Arc<dyn Any + Send + Sync + 'static>,
            instance_name,
        }
    }

    pub fn instance_name(&self) -> Option<String> {
        self.instance_name.clone()
    }
}

/// ID referencing an entity stored within the inventory.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq, Ord, PartialOrd)]
pub struct EntityID {
    num: u64,
}

impl From<EntityID> for u64 {
    fn from(id: EntityID) -> Self {
        id.num
    }
}
