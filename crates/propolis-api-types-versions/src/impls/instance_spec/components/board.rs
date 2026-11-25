// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::latest::components::board::{CpuTopology, TopoLeaves};

impl CpuTopology {
    /// A `CpuTopology` describing the initial (and unintended) structure of
    /// "single-thread-per-socket" for a VM.
    ///
    /// This further implies vCPU-many single-thread sockets, which many guests
    /// either begrudgingly tolerate or worse, particularly for high socket
    /// counts. The worst cases here are OSes with fixed 64-entry arrays where a
    /// "socket 65" or above causes stray memory accesses! This constructor is
    /// exclusively to support VMs on the "initial" virtual platform, in
    /// contrast to the "generic" virtual platform with a more normal
    /// 2-threads-per-core single-socket topology.
    pub fn legacy_single_thread_sockets(sockets: u16) -> Self {
        Self {
            has_smt: true,
            sockets,
            // The initial CPU topology as presented was even funkier than
            // simply "vCPU-many single-thread sockets"; we would report SMT
            // (above), but clear topology information from CPUID leaves Bh and
            // 8000_001Eh.
            //
            // Intel-only CPUID leaf 4h would be specialized when this initial
            // CPU topology was the default - it is only included here for
            // completeness in describing the "before explicit virtual
            // platforms" state of a propolis-server VM. The value of `std_4`
            // has no bearing on the behavior of an Oxide rack's VMs.
            topo_leaves: TopoLeaves {
                std_4: true,
                std_b: false,
                ext_8: false,
                ext_1e: false,
            },
        }
    }

    pub fn single_socket_smt() -> Self {
        Self {
            has_smt: true,
            sockets: 1,
            topo_leaves: TopoLeaves {
                std_4: true,
                std_b: true,
                ext_8: true,
                ext_1e: true,
            },
        }
    }
}
