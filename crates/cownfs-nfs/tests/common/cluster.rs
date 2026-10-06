//! Hermetic multi-server NFS cluster test harness.
//!
//! Spawns N real `cownfs-server` processes (separate address spaces, real
//! TCP) forming a single cluster. Each node gets a fresh temp image, an
//! ephemeral port, and a unique server-id. The harness wires referrals so
//! the nodes form one namespace.
//!
//! Topology for `Cluster::new_sharded(n)`:
//! - Node 0 is the frontend. Its root contains `/shard{i}` directories,
//!   each with a referral entry pointing at shard node i+1.
//! - Nodes 1..=n are shards, each holding its own data.
//!
//! A test client traverses the cluster by following NFS4ERR_MOVED +
//! fs_locations, exactly like a real client.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn server_bin() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.join("cownfs-server")
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let p = l.local_addr().unwrap().port();
    drop(l);
    p
}

fn wait_ready(addr: &SocketAddr) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        if std::net::TcpStream::connect(addr).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("cluster node did not start on {addr}");
}

/// One server process in the cluster.
pub struct ClusterNode {
    /// server-id (qualifies clientids/stateids).
    pub server_id: u32,
    pub addr: SocketAddr,
    pub img: PathBuf,
    pub uuid: [u8; 16],
    child: Child,
}

impl ClusterNode {
    fn spawn(
        dir: &std::path::Path,
        name: &str,
        server_id: u32,
        extra_args: &[&str],
    ) -> Self {
        let img = dir.join(format!("{name}.img"));
        let _ = std::fs::remove_file(&img);
        let uuid = {
            let mut fs = cownfs_core::engine::Fs::format(&img, 8192).expect("format");
            fs.commit().expect("commit");
            fs.uuid()
        };
        let port = free_port();
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let mut cmd = Command::new(server_bin());
        cmd.arg(&img)
            .arg(format!("127.0.0.1:{port}"))
            .arg("--server-id")
            .arg(server_id.to_string())
            .arg("--grace-period-secs")
            .arg("0");
        for a in extra_args {
            cmd.arg(a);
        }
        let child = cmd
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cluster node");
        wait_ready(&addr);
        Self {
            server_id,
            addr,
            img,
            uuid,
            child,
        }
    }

    /// Kill -9 the server process.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// A hermetic cluster of cownfs servers.
pub struct Cluster {
    pub nodes: Vec<ClusterNode>,
    /// Inodes of the /shard{i} dirs on the frontend (for PUTFH).
    pub shard_dir_inos: Vec<u64>,
    dir: PathBuf,
}

impl Cluster {
    /// Spawn a sharded cluster: node 0 is the frontend with `/shard{i}`
    /// referral dirs, nodes 1..=n_shards are the shards.
    pub fn new_sharded(n_shards: usize) -> Self {
        assert!(n_shards >= 1, "need at least one shard");
        let dir = std::env::temp_dir().join(format!(
            "cownfs-cluster-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // Pre-allocate ports so the frontend referral config can name them.
        let mut ports: Vec<u16> = Vec::with_capacity(n_shards + 1);
        for _ in 0..=n_shards {
            ports.push(free_port());
        }

        // Build the frontend image with /shard{i} dirs and a referral conf.
        let frontend_img = dir.join("frontend.img");
        let _ = std::fs::remove_file(&frontend_img);
        let mut shard_inos = Vec::new();
        {
            let mut fs = cownfs_core::engine::Fs::format(&frontend_img, 8192).expect("format");
            for i in 0..n_shards {
                let name = format!("shard{i}");
                let ino = fs
                    .mkdir(
                        cownfs_core::engine::ROOT_INO,
                        name.as_bytes(),
                        0o755,
                        1000,
                        1000,
                    )
                    .expect("mkdir shard dir");
                shard_inos.push(ino);
            }
            fs.commit().expect("commit");
        }
        let conf = dir.join("referrals.conf");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&conf).unwrap();
            for (i, ino) in shard_inos.iter().enumerate() {
                // Referral: <ino> <server:port> <path>
                writeln!(f, "{ino} 127.0.0.1:{} /", ports[i + 1]).unwrap();
            }
        }

        // Spawn frontend.
        let frontend_addr: SocketAddr = format!("127.0.0.1:{}", ports[0]).parse().unwrap();
        let frontend_uuid = {
            let fs = cownfs_core::engine::Fs::open(&frontend_img).expect("open frontend");
            fs.uuid()
        };
        let mut frontend_cmd = Command::new(server_bin());
        frontend_cmd
            .arg(&frontend_img)
            .arg(format!("127.0.0.1:{}", ports[0]))
            .arg("--server-id")
            .arg("0")
            .arg("--grace-period-secs")
            .arg("0")
            .arg("--referrals")
            .arg(&conf);
        let frontend_child = frontend_cmd
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn frontend");
        wait_ready(&frontend_addr);

        let mut nodes = vec![ClusterNode {
            server_id: 0,
            addr: frontend_addr,
            img: frontend_img,
            uuid: frontend_uuid,
            child: frontend_child,
        }];

        // Spawn shards.
        for i in 0..n_shards {
            // Reuse the pre-allocated port by spawning manually.
            let img = dir.join(format!("shard{i}.img"));
            let _ = std::fs::remove_file(&img);
            let uuid = {
                let mut fs = cownfs_core::engine::Fs::format(&img, 8192).expect("format");
                fs.commit().expect("commit");
                fs.uuid()
            };
            let addr: SocketAddr = format!("127.0.0.1:{}", ports[i + 1]).parse().unwrap();
            let child = Command::new(server_bin())
                .arg(&img)
                .arg(format!("127.0.0.1:{}", ports[i + 1]))
                .arg("--server-id")
                .arg((i + 1).to_string())
                .arg("--grace-period-secs")
                .arg("0")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn shard");
            wait_ready(&addr);
            nodes.push(ClusterNode {
                server_id: (i + 1) as u32,
                addr,
                img,
                uuid,
                child,
            });
        }

        Self {
            nodes,
            shard_dir_inos: shard_inos,
            dir,
        }
    }

    /// Frontend node (index 0).
    pub fn frontend(&self) -> &ClusterNode {
        &self.nodes[0]
    }

    /// Shard nodes (indices 1..).
    pub fn shards(&self) -> &[ClusterNode] {
        &self.nodes[1..]
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        for node in &mut self.nodes {
            let _ = node.child.kill();
            let _ = node.child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Decode an fs_locations attr value into (server, path) pairs.
/// fs_locations4: fl_root (pathname4) + locations<>;
/// each location: server<> + rootpath (pathname4).
pub fn decode_fs_locations(raw: &[u8]) -> Vec<(String, String)> {
    let mut pos = 0;
    let u32_at = |pos: &mut usize| -> u32 {
        let v = u32::from_be_bytes(raw[*pos..*pos + 4].try_into().unwrap());
        *pos += 4;
        v
    };
    let string_at = |pos: &mut usize| -> String {
        let len = u32_at(pos) as usize;
        let s = String::from_utf8_lossy(&raw[*pos..*pos + len]).into_owned();
        *pos += len;
        // XDR padding to 4 bytes.
        let pad = (4 - (len % 4)) % 4;
        *pos += pad;
        s
    };
    // fl_root: pathname4 (count + components).
    let ncomps = u32_at(&mut pos);
    for _ in 0..ncomps {
        string_at(&mut pos);
    }
    let nlocs = u32_at(&mut pos);
    let mut out = Vec::new();
    for _ in 0..nlocs {
        let nservers = u32_at(&mut pos);
        let mut server = String::new();
        for _ in 0..nservers {
            server = string_at(&mut pos);
        }
        let npath = u32_at(&mut pos);
        let mut comps = Vec::new();
        for _ in 0..npath {
            comps.push(string_at(&mut pos));
        }
        let path = if comps.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", comps.join("/"))
        };
        out.push((server, path));
    }
    out
}
