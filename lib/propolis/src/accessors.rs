// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Hierarchical access control for emulated resources
//!
//! Device emulation logic requires access to resources which may be
//! subsequently moderated by intervening parts of the emulation.
//!
//! For example: A PCI device performs DMA to guest memory.  If bus-mastering is
//! disabled on the device, or any parent bridge in its bus hierarchy, then its
//! contained emulation should fail any DMA accesses.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use crate::vmm::VmmHdl;

pub trait AccessedResource {
    type Root;
    type Leaf: Clone;
    type Target;

    fn derive(root: &Self::Root) -> Self::Leaf;
    fn deref(leaf: &Self::Leaf) -> &Self::Target;
}

/// Key type for identifying nodes referenced by `Tree`.
#[derive(Ord, PartialOrd, Eq, PartialEq, Debug, Copy, Clone)]
pub struct NodeKey(NonNull<Node<NodeKeyNull>>);
impl<T: AccessedResource> From<&Arc<Node<T>>> for NodeKey {
    fn from(value: &Arc<Node<T>>) -> Self {
        let raw = Arc::as_ptr(value) as *const Node<NodeKeyNull>;
        let inner =
            unsafe { NonNull::new_unchecked(raw as *mut Node<NodeKeyNull>) };
        NodeKey(inner)
    }
}
impl std::fmt::Display for NodeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:p}", self.0.as_ptr())
    }
}
// Safety: While the key uses a pointer (!Send) type internally, it is for
// unique identification purposes only, and is never meant to be dereferenced,
// copied from, or transformed into a reference of any kind.
unsafe impl Send for NodeKey {}

enum NodeKeyNull {}
impl AccessedResource for NodeKeyNull {
    type Root = ();
    type Leaf = ();
    type Target = ();
    fn derive(_root: &Self::Root) -> Self::Leaf {
        unreachable!()
    }
    fn deref(_derived: &Self::Root) -> &Self::Target {
        unreachable!()
    }
}

struct TreeNode<T: AccessedResource> {
    /// [NodeKey] of the parent to this node
    ///
    /// Holds [None] if the node is the root of the [Tree]
    parent_key: Option<NodeKey>,
    /// [Weak] reference back to the node.  This access is needed if the node
    /// undergoes adoption (being moved to a different [Tree])
    node_ref: Weak<Node<T>>,
    /// List of keys to child nodes (if any)
    children: BTreeSet<NodeKey>,
    /// Display name for [Tree::print()]-ing
    name: Option<String>,
}
impl<T: AccessedResource> TreeNode<T> {
    fn new(
        parent_key: NodeKey,
        node_ref: Weak<Node<T>>,
        name: Option<String>,
    ) -> Self {
        Self {
            parent_key: Some(parent_key),
            node_ref,
            children: BTreeSet::new(),
            name,
        }
    }
    fn new_root(node_ref: Weak<Node<T>>) -> Self {
        Self {
            parent_key: None,
            node_ref,
            children: BTreeSet::new(),
            name: None,
        }
    }
}

struct Tree<T: AccessedResource> {
    /// Root resource (if any) that this hierarchy is granting access to
    res_root: Option<T::Root>,

    /// Key of the root node of this hierarchy
    ///
    /// Only when the tree is being initialized, should `root_key` be [None]
    root_key: Option<NodeKey>,

    /// Nodes within this hierarchy
    nodes: BTreeMap<NodeKey, TreeNode<T>>,

    /// Weak self-reference, used when building [TreeNode] entries as nodes are
    /// added to the tree.  Held as a convenience, instead of requiring it to be
    /// passed in by the caller.
    self_weak: Weak<Mutex<Tree<T>>>,
}
impl<T: AccessedResource> Tree<T> {
    /// Record a node in the tree
    fn add_child(
        &mut self,
        parent: NodeKey,
        name: Option<String>,
    ) -> Arc<Node<T>> {
        let child_node = Arc::new(Node(Mutex::new(NodeEntry {
            tree: Weak::upgrade(&self.self_weak).expect("tree ref still live"),
            res_leaf: self.res_root.as_ref().map(T::derive),
        })));

        let child_key = NodeKey::from(&child_node);
        let conflict = self.nodes.insert(
            child_key,
            TreeNode::new(parent, Arc::downgrade(&child_node), name),
        );
        assert!(
            conflict.is_none(),
            "new child should not conflict with existing node"
        );

        self.nodes
            .get_mut(&parent)
            .expect("parent node must exist")
            .children
            .insert(child_key);

        child_node
    }

    /// Adopt the root node and all its descendants into our tree, under the
    /// node specified by `parent_key`
    fn adopt(&mut self, parent_key: NodeKey, adopt_tree: &mut Tree<T>) {
        debug_assert!(
            self.nodes
                .get(&parent_key)
                .and_then(|node| Weak::upgrade(&node.node_ref))
                .is_some(),
            "leaf target for re-parenting missing"
        );

        let child_key = adopt_tree.root_key();
        let tree_ref = self.self_weak.upgrade().unwrap();

        let mut queue = VecDeque::new();
        queue.push_back(child_key);
        while let Some(adopt_key) = queue.pop_front() {
            if let Some(mut tnode) = adopt_tree.nodes.remove(&adopt_key) {
                let node = match Weak::upgrade(&tnode.node_ref) {
                    Some(nr) => nr,
                    None => {
                        continue;
                    }
                };

                // Associate the node with this tree and resource
                {
                    let mut ent = node.0.lock().unwrap();
                    ent.tree = Arc::clone(&tree_ref);
                    ent.res_leaf = self.res_root.as_ref().map(T::derive);
                }

                if adopt_key == child_key {
                    // The root of the adopted tree needs its parent set (and to
                    // be added to the children list of said parent.
                    //
                    // All of the descendant nodes will have those relationships
                    // properly established when they are copied over.
                    tnode.parent_key = Some(parent_key);
                    let parent_node = self
                        .nodes
                        .get_mut(&parent_key)
                        .expect("parent node is present");
                    parent_node.children.insert(adopt_key);
                }

                queue.extend(tnode.children.iter());

                let _conflict = self.nodes.insert(adopt_key, tnode);
                assert!(_conflict.is_none());
            }
        }
        debug_assert!(adopt_tree.nodes.is_empty());
    }

    /// Remove traces of a node from the tree as it is dropped
    fn remove_dead_node(&mut self, key: NodeKey) {
        let mut tnode =
            self.nodes.remove(&key).expect("tree node should be present");

        if let Some(pkey) = tnode.parent_key.as_ref() {
            let was_removed = self
                .nodes
                .get_mut(pkey)
                .expect("parent for node exists")
                .children
                .remove(&key);
            assert!(was_removed, "parent should list node as child");
        } else {
            assert_eq!(
                Some(key),
                self.root_key,
                "node without parent must be tree root"
            );
        }

        // orphan any children of the node
        for child in std::mem::take(&mut tnode.children) {
            self.orphan_node(child);
        }
    }

    /// Remove a node from this Tree into a new empty tree, with all of its
    /// descendants in tow.
    fn orphan_node(&mut self, key: NodeKey) {
        let mut tnode =
            self.nodes.remove(&key).expect("node-to-orphan is present in tree");

        let orphan_tree = Self::new_empty(None);

        // This node now becomes the root of the orphaned tree
        {
            let node =
                tnode.node_ref.upgrade().expect("node-to-orphan is still live");
            let mut guard = node.0.lock().unwrap();
            guard.tree = orphan_tree.clone();
            guard.res_leaf.take();
        }
        tnode.parent_key = None;

        let mut needs_moved = VecDeque::new();
        needs_moved.extend(tnode.children.iter());

        let mut tguard = orphan_tree.lock().unwrap();
        tguard.root_key = Some(key);
        tguard.nodes.insert(key, tnode);

        while let Some(move_key) = needs_moved.pop_front() {
            let tnode = self
                .nodes
                .remove(&move_key)
                .expect("child tree node is present");

            // Progeny of the orphaned node which are still "live" need to be
            // associated with the new tree.  Anything which happens to be
            // "dead" will clean itself from the existing tree and orphan its
            // subsequent progeny when given access to the tree lock.
            if let Some(node) = tnode.node_ref.upgrade() {
                let mut ent = node.0.lock().unwrap();
                ent.tree = orphan_tree.clone();
                ent.res_leaf = None;

                needs_moved.extend(tnode.children.iter());

                tguard.nodes.insert(move_key, tnode);
            }
        }
    }

    /// Set the string name of node specified by `key`
    fn rename_node(&mut self, key: NodeKey, name: Option<String>) {
        if let Some(tnode) = self.nodes.get_mut(&key) {
            tnode.name = name;
        }
    }

    /// Returns `true` if a given `node` is the root of this tree
    fn node_is_root(&self, node: &Arc<Node<T>>) -> bool {
        self.root_key() == node.into()
    }

    fn set_root_resource(
        &mut self,
        new_root: Option<T::Root>,
    ) -> Option<T::Root> {
        // Swap out the existing root resource
        let old = std::mem::replace(&mut self.res_root, new_root);

        // ... and invalidate all nodes too
        for tnode in self.nodes.values() {
            if let Some(node) = tnode.node_ref.upgrade() {
                let _ = node.0.lock().unwrap().res_leaf.take();
            }
        }

        old
    }

    /// How many nodes exist in this tree hierarchy?
    fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Traverse tree in order conducive to printing, applying a provided
    /// `print_fn` to each node.
    fn print(&self, print_fn: impl Fn(PrintNode)) {
        // Seed the root of the tree to be processed at depth 0
        let mut initial = BTreeSet::new();
        let root_key = self.root_key();
        initial.insert(root_key);
        let mut to_process = vec![(0, initial)];

        while let Some((depth, mut children)) = to_process.pop() {
            let key = match children.pop_first() {
                Some(i) => {
                    to_process.push((depth, children));
                    i
                }
                None => continue,
            };

            if let Some(tnode) = self.nodes.get(&key) {
                let pnode = PrintNode {
                    depth,
                    key,
                    is_root: key == root_key,
                    name: tnode.name.as_deref(),
                };
                print_fn(pnode);
                if !tnode.children.is_empty() {
                    to_process.push((depth + 1, tnode.children.clone()))
                }
            }
        }
    }

    /// Get the [NodeKey] of the tree root
    ///
    /// Panics if called before the tree is initialized.
    fn root_key(&self) -> NodeKey {
        self.root_key.expect("root_key is non-None once tree is initialized")
    }

    /// Create a [Tree] with no nodes (not even a root)
    fn new_empty(primary: Option<T::Root>) -> Arc<Mutex<Tree<T>>> {
        Arc::new_cyclic(|self_weak| {
            Mutex::new(Tree {
                res_root: primary,
                nodes: BTreeMap::new(),
                root_key: None,
                self_weak: self_weak.clone(),
            })
        })
    }

    /// Create a [Tree] returning the root node
    fn new(res_root: Option<T::Root>) -> Arc<Node<T>> {
        let res_leaf = res_root.as_ref().map(T::derive);
        let tree = Self::new_empty(res_root);
        let node = Node::new_root(tree.clone());
        node.0.lock().unwrap().res_leaf = res_leaf;

        let mut guard = tree.lock().unwrap();
        let root_key = NodeKey::from(&node);
        guard.root_key = Some(root_key);
        guard.nodes.insert(root_key, TreeNode::new_root(Arc::downgrade(&node)));

        node
    }
}

/// Data provided to `print_fn` callback as part of `Tree::print()`
pub struct PrintNode<'a> {
    pub depth: usize,
    pub key: NodeKey,
    pub is_root: bool,
    pub name: Option<&'a str>,
}

/// Build printing function for [`Tree::print()`] which outputs a list format.
fn print_basic(match_node: Option<NodeKey>) -> impl Fn(PrintNode) {
    move |node| {
        let key = node.key;
        let pad = "  ".repeat(node.depth);
        let highlight = if Some(key) == match_node { " ***" } else { "" };
        let namestr = match node.name {
            None if node.is_root => "'ROOT'".to_string(),
            None => "<unnamed>".to_string(),
            Some(s) => format!("'{s}'"),
        };

        println!("{pad}- {{ id: {key:#}, name: {namestr} }}{highlight}");
    }
}

type TreeBackref<T> = Arc<Mutex<Tree<T>>>;

struct NodeEntry<T: AccessedResource> {
    tree: TreeBackref<T>,
    /// Leaf resource for this node in the tree.
    ///
    /// The contents of the leaf resource may differ between nodes, as it is
    /// effectively a cache of the [AccessedResource::derive()] output, when not
    /// cleared as part of invalidation from the root.
    res_leaf: Option<T::Leaf>,
    // TODO: store enable/disable state here for evaluation and propagation
}
struct Node<T: AccessedResource>(Mutex<NodeEntry<T>>);
impl<T: AccessedResource> Node<T> {
    /// Lock tree and entry (in that order, as required), and check if the tree
    /// we locked is the one this node is associated with.
    ///
    /// If the tree references match, the two guards are returned. If not, the
    /// tree to which we are now associated is returned instead.
    ///
    /// This is purely a helper function to make lifetimes clearer for
    /// [`Self::lock_tree()`]
    #[allow(clippy::type_complexity)]
    fn try_lock_tree<'a>(
        &'a self,
        tree_ref: &'a TreeBackref<T>,
    ) -> Result<MutexGuard<'a, Tree<T>>, TreeBackref<T>> {
        let guard = tree_ref.lock().unwrap();
        let node_guard = self.0.lock().unwrap();
        if Arc::ptr_eq(tree_ref, &node_guard.tree) {
            Ok(guard)
        } else {
            Err(node_guard.tree.clone())
        }
    }

    /// Safely acquire the lock to this entry, as well as the containing tree,
    /// respecting the ordering requirements.
    fn lock_tree<R>(&self, f: impl FnOnce(MutexGuard<'_, Tree<T>>) -> R) -> R {
        let mut tree = self.0.lock().unwrap().tree.clone();
        let guard = loop {
            let new_tree = match self.try_lock_tree(&tree) {
                Ok(tg) => break tg,
                Err(nt) => nt,
            };
            let _ = std::mem::replace(&mut tree, new_tree);
        };
        f(guard)
    }

    fn new_root(tree: Arc<Mutex<Tree<T>>>) -> Arc<Node<T>> {
        Arc::new(Node(Mutex::new(NodeEntry { tree, res_leaf: None })))
    }
    fn new_child(self: &Arc<Node<T>>, name: Option<String>) -> Arc<Node<T>> {
        self.lock_tree(|mut guard| guard.add_child(self.into(), name))
    }

    fn guard(&self) -> Option<Guard<'_, T>> {
        let local = self.0.lock().unwrap();
        if let Some(leaf) = local.res_leaf.as_ref() {
            Some(Guard { inner: leaf.clone(), _pd: PhantomData })
        } else {
            drop(local);
            // Attempt to (re)derive leaf resource from root
            self.lock_tree(|tree| {
                if let Some(root) = tree.res_root.as_ref() {
                    let mut local = self.0.lock().unwrap();
                    let leaf = T::derive(root);
                    local.res_leaf = Some(leaf.clone());

                    Some(Guard { inner: leaf, _pd: PhantomData })
                } else {
                    None
                }
            })
        }
    }

    fn drop_from_tree(self: &mut Arc<Node<T>>) {
        let key = NodeKey::from(&*self);
        self.lock_tree(|mut guard| {
            // drop any lingering access to the resource immediately
            let _ = self.0.lock().unwrap().res_leaf.take();

            // Since we hold the Tree lock (thus eliminating the chance of any
            // racing adopt/orphan activity to be manipulating the refcount on
            // the `Arc<Node<T>>`, we expect that its strong count is exactly 1.
            //
            // We, the holder (as part of Accessor::drop()) are the only one
            // with a strong reference, which should be released momentarily.
            debug_assert_eq!(Arc::strong_count(self), 1);

            guard.remove_dead_node(key);
        });
    }
}

pub struct Guard<'a, T: AccessedResource> {
    inner: T::Leaf,
    _pd: PhantomData<&'a T>,
}
impl<T: AccessedResource> std::ops::Deref for Guard<'_, T> {
    type Target = T::Target;
    fn deref(&self) -> &Self::Target {
        T::deref(&self.inner)
    }
}

pub struct Accessor<T: AccessedResource>(Arc<Node<T>>);
impl<T: AccessedResource> Accessor<T> {
    /// Create a new accessor hierarchy, mediating access to `resource`.
    pub fn new(resource: T::Root) -> Self {
        Self(Tree::new(Some(resource)))
    }

    /// Create a new orphaned accessor hierarchy, bearing no existing resource.
    /// The hierarchy can gain access to a valid resource by being
    /// [adopted][`Self::adopt()`].
    pub fn new_orphan() -> Self {
        Self(Tree::new(None))
    }

    /// Create a child of this node.
    pub fn child(&self, name: Option<String>) -> Self {
        Self(self.0.new_child(name))
    }

    /// Adopt an orphan node and its descendants.
    ///
    /// # Panics
    ///
    /// If the node to be adopted is not the root of an orphan tree.
    pub fn adopt(&self, child: &Self, name: Option<String>) {
        let parent_key = NodeKey::from(&self.0);
        let child_key = NodeKey::from(&child.0);

        assert_ne!(parent_key, child_key, "cannot adopt self");

        self.0.lock_tree(|mut parent_guard| {
            child.0.lock_tree(|mut child_guard| {
                if !child_guard.node_is_root(&child.0) {
                    // Drop all mutex guards prior to panic in order to allow
                    // unwinder to do its job, rather than getting tripped up by
                    // poisoned mutexes.  This allows the unit tests to exercise
                    // this panic condition.
                    drop(child_guard);
                    drop(parent_guard);
                    panic!("adopting of non-roots not allowed");
                }
                // Apply the chosen name to the root prior to its adoption
                child_guard.rename_node(child_key, name);
                parent_guard.adopt(parent_key, &mut child_guard);
            });
        });
    }

    /// Remove the underlying resource from the root node of a hierarchy.  This
    /// is meant to provide the root holder of the resource the means to
    /// promptly remove access to it during events such as tear-down.
    ///
    /// # Panics
    ///
    /// If this is called on a non-root node.
    pub fn remove_resource(&self) -> Option<T::Root> {
        self.0.lock_tree(|mut guard| {
            if !guard.node_is_root(&self.0) {
                drop(guard);
                panic!("removal of root resource only allowed at root node");
            }

            guard.set_root_resource(None)
        })
    }

    /// Attempt to gain access to the underlying resource.
    ///
    /// Will return [None] if any ancestor node disables access, or if the node
    /// is not attached to a hierarchy containing a valid resource.
    pub fn access(&self) -> Option<Guard<'_, T>> {
        self.0.guard()
    }

    /// How many nodes exist in this Accessor hierarchy
    pub fn node_count(&self) -> usize {
        self.0.lock_tree(|guard| guard.node_count())
    }

    /// Print the hierarchy that this node is a member of
    pub fn print(&self, highlight_self: bool) {
        self.0.lock_tree(|tree| {
            tree.print(print_basic(
                highlight_self.then_some(NodeKey::from(&self.0)),
            ));
        });
    }
}
impl<T: AccessedResource> Drop for Accessor<T> {
    /// Perform necessary `Node` clean-up in the containing tree during drop of
    /// the Accessor.
    ///
    /// On first glance it would seem like this logic belongs in the [Drop] impl
    /// for `Node`, rather than the [Accessor].  Doing it that way mostly works,
    /// but poses a challenge: When the `Tree` is performing node adoption or
    /// orphaning, it must reach back out into the Nodes it owns via
    /// `TreeNode::node_ref`.  This poses a race for when the last
    /// `Arc<Node<T>>` is dropped, either by the `Accessor`, or the logic in the
    /// `Tree`.  When the latter wins, it still holds the tree lock, posing a
    /// deadlock situation which is otherwise impossible to prevent.
    ///
    /// As such it is expected that the Accessor will perform the tree
    /// de-registration of the node when it is being dropped.  No other
    /// structures should hold the `Arc<Node<T>>`.
    fn drop(&mut self) {
        self.0.drop_from_tree();
    }
}

pub type MemAccessor = Accessor<crate::vmm::mem::MemAccessed>;

enum MsiAccessed {}
impl AccessedResource for MsiAccessed {
    type Root = Arc<VmmHdl>;
    type Leaf = Arc<VmmHdl>;
    type Target = VmmHdl;

    fn derive(root: &Self::Root) -> Self::Leaf {
        root.clone()
    }
    fn deref(leaf: &Self::Leaf) -> &Self::Target {
        leaf
    }
}

// Keep the rest of VmmHdl hidden for the MSI accessor
pub struct MsiAccessor(Accessor<MsiAccessed>);
impl MsiAccessor {
    /// See: [`Accessor::new()`]
    pub fn new(hdl: Arc<VmmHdl>) -> Self {
        Self(Accessor::new(hdl))
    }
    /// See: [`Accessor::new_orphan()`]
    pub fn new_orphan() -> Self {
        Self(Accessor::new_orphan())
    }
    /// See: [`Accessor::child()`]
    pub fn child(&self, name: Option<String>) -> Self {
        Self(self.0.child(name))
    }
    /// See: [`Accessor::adopt()`]
    pub fn adopt(&self, child: &Self, name: Option<String>) {
        self.0.adopt(&child.0, name)
    }
    /// See: [Accessor::remove_resource()]
    pub fn remove_resource(&self) -> Option<Arc<VmmHdl>> {
        self.0.remove_resource()
    }

    /// Attempt to send an MSI with the resource held by this accessor
    /// hierarchy.  Returns [`Ok`] if valid access to the resource exists,
    /// otherwise [`Err`].
    pub fn send(&self, addr: u64, msg: u64) -> Result<(), ()> {
        if let Some(guard) = self.0.access() {
            guard.lapic_msi(addr, msg).expect("lapic_msi() should succeed");
            Ok(())
        } else {
            Err(())
        }
    }
}
impl std::fmt::Debug for MsiAccessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MsiAccessor").finish()
    }
}

#[cfg(test)]
mod test {
    //! Note regarding unwinding for `should_panic` tests:
    //!
    //! If any mutexes are held when the code under test panics, the poisoned
    //! mutex will prevent the unwinder from functioning properly when the
    //! [MutexGuard]s are dropped.  You will see several checks in the above
    //! logic which eschew [assert_eq] for a manual check which drops any held
    //! mutexes before issuing a [panic].

    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    enum AtomicRes {}
    impl AccessedResource for AtomicRes {
        type Root = Arc<AtomicUsize>;
        type Leaf = Arc<AtomicUsize>;
        type Target = AtomicUsize;

        fn derive(root: &Self::Root) -> Self::Leaf {
            root.clone()
        }
        fn deref(leaf: &Self::Leaf) -> &Self::Target {
            leaf
        }
    }

    // Helpers:

    fn new_root() -> Accessor<AtomicRes> {
        Accessor::new(Arc::new(AtomicUsize::new(0)))
    }
    fn new_orphan() -> Accessor<AtomicRes> {
        Accessor::new_orphan()
    }
    fn new_depth(
        depth: usize,
    ) -> (Accessor<AtomicRes>, Vec<Accessor<AtomicRes>>) {
        let root = new_root();
        let mut children: Vec<Accessor<AtomicRes>> = Vec::with_capacity(depth);

        for idx in 0..depth {
            let next_child = match idx {
                0 => root.child(None),
                n => children[n - 1].child(None),
            };
            children.push(next_child);
        }
        (root, children)
    }

    #[test]
    fn tree_root() {
        let root = new_root();

        let guard = root.access();
        assert!(guard.is_some());
        let guard = guard.unwrap();
        drop(guard);

        let res = root.remove_resource();
        assert!(res.is_some());

        assert!(root.access().is_none())
    }

    #[test]
    fn simple_orphan() {
        let root = new_root();
        let orphan = new_orphan();

        assert!(root.access().is_some());
        assert!(orphan.access().is_none());

        root.adopt(&orphan, None);
        assert!(orphan.access().is_some());
    }

    #[test]
    #[should_panic]
    fn only_root_can_remove_resource() {
        let root = new_root();
        let child = root.child(None);

        assert!(root.access().is_some());
        assert!(child.access().is_some());

        child.remove_resource();
    }

    #[test]
    #[should_panic]
    fn adopt_self() {
        let root = new_root();
        root.adopt(&root, None);
    }

    #[test]
    #[should_panic]
    fn adopt_nonroot() {
        let root = new_root();
        let child = new_orphan();
        let grandchild = child.child(None);

        root.adopt(&grandchild, None);
    }

    #[test]
    fn simple_depth() {
        let depth = 4;
        let (root, children) = new_depth(depth);

        // update the inner resource, checking that it's the same at all depths
        let tval = 1;
        root.access().unwrap().store(tval, Ordering::Relaxed);
        for child in children.iter() {
            assert_eq!(child.access().unwrap().load(Ordering::Relaxed), tval);
        }

        root.remove_resource();
        for node in children.iter() {
            assert!(node.access().is_none());
        }
    }

    #[test]
    fn orphan_split() {
        let (root, children) = new_depth(5);

        // Wrap the children in Option, so we can drop one from the middle to
        // orphan its descendants
        let mut children =
            children.into_iter().map(|c| Some(c)).collect::<Vec<Option<_>>>();

        // Drop the middle node, causing its children to become orphaned
        children[2] = None;

        // Children above the "split" should be fine
        for child in children[0..2].iter().map(|c| c.as_ref().unwrap()) {
            assert!(child.access().is_some());
        }

        // Those below should be orphaned, with no access to the resource
        for child in children[3..].iter().map(|c| c.as_ref().unwrap()) {
            assert!(child.access().is_none());
        }

        let tval = 1;
        root.access().unwrap().store(tval, Ordering::Relaxed);

        // Closest available child will adopt the orphan chain
        children[1]
            .as_ref()
            .unwrap()
            .adopt(children[3].as_ref().unwrap(), None);

        // The adopted nodes should have access (and see the updated val)
        for child in children[3..].iter().map(|c| c.as_ref().unwrap()) {
            let guard = child.access().expect("resource is accessible");
            assert_eq!(guard.load(Ordering::Relaxed), tval);
        }
    }

    #[test]
    fn orphan_sibling() {
        let (root, mut children) = new_depth(2);

        let sib = root.child(Some("sibling".to_string()));
        let sib_child = sib.child(Some("sibling child".to_string()));

        // Check that both siblings, and their progeny, can access the resource
        assert!(sib.access().is_some());
        assert!(children[0].access().is_some());
        assert!(sib_child.access().is_some());
        assert!(children[1].access().is_some());

        // ... and that after orphaning one of them, that the other sibling
        // still has access
        let _ = children.remove(0);
        assert!(children[0].access().is_none());
        assert!(sib.access().is_some());
        assert!(sib_child.access().is_some());
    }

    #[test]
    fn print_names() {
        let root = new_root();

        // build up an arbitrary hierarchy to print out
        let left = root.child(Some("left".to_string()));
        let right = root.child(Some("right".to_string()));
        let mut sub = Vec::new();
        for n in 0..4 {
            let lsub = left.child(Some(format!("sub {n}")));
            let rsub = right.child(Some(format!("sub {n}")));

            match n {
                3 => {
                    sub.push(lsub.child(Some("deep".to_string())));
                }
                2 => {
                    sub.push(rsub.child(Some("deep".to_string())));
                }
                _ => {}
            }
            sub.push(lsub);
            sub.push(rsub);
        }

        right.print(true);
    }
}
