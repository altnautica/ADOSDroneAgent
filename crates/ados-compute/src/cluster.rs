//! The master/slave cluster view.
//!
//! There is always one master compute node (the single logical endpoint a drone
//! or GCS pairs with, and the scheduler). Extra nodes slave to it and offer
//! their workers. v1 ships master-only (a lone node is the master); this struct
//! is the master's view of the cluster, with the slave-registration seam ready
//! for the distributed scheduler. A slave promotes to master on master loss
//! (the election/failover layer is reused from the fleet precedents).

use crate::{ClusterDescriptor, ComputeRole, SlaveDescriptor};

/// The master node's view of its cluster.
#[derive(Debug, Clone)]
pub struct Cluster {
    master_id: String,
    slaves: Vec<SlaveDescriptor>,
}

impl Cluster {
    /// A fresh cluster with this node as the master and no slaves.
    pub fn new_master(master_id: impl Into<String>) -> Self {
        Self {
            master_id: master_id.into(),
            slaves: Vec::new(),
        }
    }

    /// This node's role. The master's view is always [`ComputeRole::Master`].
    pub fn role(&self) -> ComputeRole {
        ComputeRole::Master
    }

    pub fn master_id(&self) -> &str {
        &self.master_id
    }

    /// Register a slave (or refresh it if the `node_id` is already known). A
    /// slave registering twice updates its advertised capacity rather than
    /// duplicating the entry.
    pub fn register_slave(&mut self, slave: SlaveDescriptor) {
        if let Some(existing) = self.slaves.iter_mut().find(|s| s.node_id == slave.node_id) {
            *existing = slave;
        } else {
            self.slaves.push(slave);
        }
    }

    /// Drop a slave that left or was lost. Returns whether one was removed.
    pub fn remove_slave(&mut self, node_id: &str) -> bool {
        let before = self.slaves.len();
        self.slaves.retain(|s| s.node_id != node_id);
        self.slaves.len() != before
    }

    pub fn slaves(&self) -> &[SlaveDescriptor] {
        &self.slaves
    }

    /// Total idle workers across this master and every registered slave.
    pub fn aggregate_workers_idle(&self, master_workers_idle: u32) -> u32 {
        master_workers_idle + self.slaves.iter().map(|s| s.workers_idle).sum::<u32>()
    }

    /// The wire descriptor for the heartbeat, given the master's own idle count.
    pub fn descriptor(&self, master_workers_idle: u32) -> ClusterDescriptor {
        ClusterDescriptor {
            master_id: self.master_id.clone(),
            slaves: self.slaves.clone(),
            aggregate_workers_idle: self.aggregate_workers_idle(master_workers_idle),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slave(id: &str, idle: u32) -> SlaveDescriptor {
        SlaveDescriptor {
            node_id: id.into(),
            accelerators: vec!["cuda:0".into()],
            workers_idle: idle,
            queue_depth: 0,
        }
    }

    #[test]
    fn lone_node_is_master_with_no_slaves() {
        let c = Cluster::new_master("node-a");
        assert_eq!(c.role(), ComputeRole::Master);
        assert_eq!(c.master_id(), "node-a");
        assert!(c.slaves().is_empty());
        assert_eq!(c.aggregate_workers_idle(2), 2);
    }

    #[test]
    fn register_dedups_and_refreshes() {
        let mut c = Cluster::new_master("node-a");
        c.register_slave(slave("node-b", 1));
        c.register_slave(slave("node-c", 2));
        c.register_slave(slave("node-b", 4)); // refresh, not duplicate
        assert_eq!(c.slaves().len(), 2);
        // master 2 + b 4 + c 2 = 8
        assert_eq!(c.aggregate_workers_idle(2), 8);
        let d = c.descriptor(2);
        assert_eq!(d.master_id, "node-a");
        assert_eq!(d.aggregate_workers_idle, 8);
    }

    #[test]
    fn remove_slave_on_loss() {
        let mut c = Cluster::new_master("node-a");
        c.register_slave(slave("node-b", 1));
        assert!(c.remove_slave("node-b"));
        assert!(!c.remove_slave("node-b"));
        assert!(c.slaves().is_empty());
    }
}
