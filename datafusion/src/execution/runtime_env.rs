// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Execution runtime environment that tracks memory, disk and various configurations
//! that are used during physical plan execution.

use crate::error::Result;
use crate::execution::disk_manager::DiskManager;
use crate::execution::memory_management::{MemoryConsumer, MemoryManager};
use lazy_static::lazy_static;
use std::sync::Arc;

lazy_static! {
    /// Employ lazy static temporarily for RuntimeEnv, to avoid plumbing it through
    /// all `async fn execute(&self, partition: usize, runtime: Arc<RuntimeEnv>)`
    pub static ref RUNTIME_ENV: Arc<RuntimeEnv> = {
        let config = RuntimeConfig::new();
        Arc::new(RuntimeEnv::new(config).unwrap())
    };
}

#[derive(Clone)]
/// Execution runtime environment
pub struct RuntimeEnv {
    /// Runtime configuration
    pub config: RuntimeConfig,
    /// Runtime memory management
    pub memory_manager: Arc<MemoryManager>,
    /// Manage temporary files during query execution
    pub disk_manager: Arc<DiskManager>,
}

impl RuntimeEnv {
    /// Create env based on configuration
    pub fn new(config: RuntimeConfig) -> Result<Self> {
        let memory_manager = Arc::new(MemoryManager::new(config.max_memory));
        let disk_manager = Arc::new(DiskManager::new(&config.local_dirs)?);
        Ok(Self {
            config,
            memory_manager,
            disk_manager,
        })
    }

    /// Get execution batch size based on config
    pub fn batch_size(&self) -> usize {
        self.config.batch_size
    }

    /// Register the consumer to get it tracked
    pub async fn register_consumer(&self, memory_consumer: Arc<dyn MemoryConsumer>) {
        self.memory_manager.register_consumer(memory_consumer).await;
    }
}

#[derive(Clone)]
/// Execution runtime configuration
pub struct RuntimeConfig {
    /// Default batch size when creating new batches
    pub batch_size: usize,
    /// Max execution memory allowed for DataFusion
    pub max_memory: usize,
    /// Local dirs to store temporary files during execution
    pub local_dirs: Vec<String>,
}

impl RuntimeConfig {
    /// New with default values
    pub fn new() -> Self {
        Default::default()
    }

    /// Customize batch size
    pub fn with_batch_size(mut self, n: usize) -> Self {
        // batch size must be greater than zero
        assert!(n > 0);
        self.batch_size = n;
        self
    }

    /// Customize exec size
    pub fn with_max_execution_memory(mut self, max_memory: usize) -> Self {
        assert!(max_memory > 0);
        self.max_memory = max_memory;
        self
    }

    /// Customize exec size
    pub fn with_local_dirs(mut self, local_dirs: Vec<String>) -> Self {
        assert!(!local_dirs.is_empty());
        self.local_dirs = local_dirs;
        self
    }
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        let tmp_dir = tempfile::tempdir().unwrap();
        let path = tmp_dir.path().to_str().unwrap().to_string();
        std::mem::forget(tmp_dir);

        Self {
            batch_size: 8192,
            max_memory: usize::MAX,
            local_dirs: vec![path],
        }
    }
}
