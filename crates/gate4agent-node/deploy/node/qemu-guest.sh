#!/bin/sh
# Mode 3 — node as a QEMU guest.
# Builds a minimal Alpine initramfs, boots it, and mounts a prepared directory
# over virtio-9p. The kit binary, its loader and libraries, iproute2, and
# wireguard-tools come from that mount. The QEMU NIC is user-mode or a tap.
# It is only the underlay. Kernel WireGuard is not a QEMU network: the node
# process creates it when the guest environment sets GATE4AGENT_WG_*, the same
# as mode 1, and dials C2 over that tunnel.
set -eu
HERE=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
REPO=$(CDPATH= cd -- "$HERE/../../../.." && pwd)
KIT_BIN=${KIT_BIN:-$REPO/target/release/gate4agent-node}
WORKDIR=${WORKDIR:-$REPO/target/qemu-guest}
SHARE=${SHARE:-$WORKDIR/share}
GUEST_MEM=${GUEST_MEM:-256}
QEMU_NET=${QEMU_NET:-user}
TAP_IF=${TAP_IF:-}
QEMU_NETNS=${QEMU_NETNS:-}
CONSOLE_LOG=${CONSOLE_LOG:-$WORKDIR/console.log}
GUEST_ENV_FILE=${GUEST_ENV_FILE:-}
ALPINE_VERSION=${ALPINE_VERSION:-3.20.3}
ALPINE_MIRROR=${ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine/v3.20}
QEMU_KERNEL=${QEMU_KERNEL:-}
QEMU_MODLOOP=${QEMU_MODLOOP:-}
APK_DIR=${APK_DIR:-}
MINIROOTFS=${MINIROOTFS:-}
FOREGROUND=${FOREGROUND:-0}
if [ "$(id -u)" -ne 0 ]; then
  exec sudo --preserve-env=KIT_BIN,WORKDIR,SHARE,GUEST_MEM,QEMU_NET,TAP_IF,QEMU_NETNS,CONSOLE_LOG,GUEST_ENV_FILE,ALPINE_VERSION,ALPINE_MIRROR,QEMU_KERNEL,QEMU_MODLOOP,APK_DIR,MINIROOTFS,FOREGROUND,GUEST_ADDR,GUEST_PREFIX,GUEST_GW,QEMU_ACCEL \
    "$0" "$@"
fi
mkdir -p "$WORKDIR" "$SHARE" "$(dirname "$CONSOLE_LOG")"
: > "$CONSOLE_LOG"
if [ ! -x "$KIT_BIN" ]; then
  echo "kit binary is missing: $KIT_BIN" | tee -a "$CONSOLE_LOG"
  exit 1
fi
case "$QEMU_NET" in
  user)
    GUEST_ADDR=${GUEST_ADDR:-10.0.2.15}
    GUEST_PREFIX=${GUEST_PREFIX:-24}
    GUEST_GW=${GUEST_GW:-10.0.2.2}
    ;;
  tap)
    if [ -z "$TAP_IF" ]; then
      echo "QEMU_NET=tap requires TAP_IF" | tee -a "$CONSOLE_LOG"
      exit 1
    fi
    GUEST_ADDR=${GUEST_ADDR:-192.168.81.10}
    GUEST_PREFIX=${GUEST_PREFIX:-24}
    GUEST_GW=${GUEST_GW:-192.168.81.1}
    ;;
  *)
    echo "QEMU_NET must be user or tap" | tee -a "$CONSOLE_LOG"
    exit 1
    ;;
esac
CACHE=$WORKDIR/cache
ROOT=$WORKDIR/rootfs
MODROOT=$WORKDIR/modules
mkdir -p "$CACHE" "$MODROOT"
rm -rf "$ROOT"
mkdir -p "$ROOT"
if [ -z "$MINIROOTFS" ]; then
  MINIROOTFS=$CACHE/alpine-minirootfs-${ALPINE_VERSION}-x86_64.tar.gz
fi
if [ ! -s "$MINIROOTFS" ]; then
  curl -fL --retry 3 -o "$MINIROOTFS" \
    "$ALPINE_MIRROR/releases/x86_64/alpine-minirootfs-${ALPINE_VERSION}-x86_64.tar.gz"
fi
tar -C "$ROOT" -xzf "$MINIROOTFS"
if [ -z "$QEMU_KERNEL" ] || [ -z "$QEMU_MODLOOP" ]; then
  NB=$CACHE/alpine-netboot-${ALPINE_VERSION}-x86_64.tar.gz
  if [ ! -s "$NB" ]; then
    curl -fL --retry 3 -o "$NB" \
      "$ALPINE_MIRROR/releases/x86_64/alpine-netboot-${ALPINE_VERSION}-x86_64.tar.gz"
  fi
  if [ -z "$QEMU_KERNEL" ] || [ ! -s "$QEMU_KERNEL" ]; then
    tar -C "$CACHE" -xzf "$NB" boot/vmlinuz-virt
    QEMU_KERNEL=$CACHE/boot/vmlinuz-virt
  fi
  if [ -z "$QEMU_MODLOOP" ] || [ ! -s "$QEMU_MODLOOP" ]; then
    tar -C "$CACHE" -xzf "$NB" boot/modloop-virt
    QEMU_MODLOOP=$CACHE/boot/modloop-virt
  fi
fi
MNT=$WORKDIR/modloop-mnt
mkdir -p "$MNT"
CLEAN_MODLOOP=0
if ! mountpoint -q "$MNT"; then
  mount -o loop,ro "$QEMU_MODLOOP" "$MNT"
  CLEAN_MODLOOP=1
fi
python3 - "$MNT" "$MODROOT" << 'ENDMOD'
import sys
from pathlib import Path
mnt, dest = Path(sys.argv[1]), Path(sys.argv[2])
dep_file = next(mnt.glob("modules/*/modules.dep"))
base = dep_file.parent
graph = {}
for line in dep_file.read_text().splitlines():
    if ":" not in line:
        continue
    left, right = line.split(":", 1)
    graph[left.strip()] = [x for x in right.split() if x]
seeds = [
    "kernel/drivers/net/wireguard/wireguard.ko",
    "kernel/drivers/net/virtio_net.ko",
    "kernel/fs/9p/9p.ko",
    "kernel/net/9p/9pnet_virtio.ko",
]
ordered, vis = [], set()
def walk(name):
    if name in vis:
        return
    vis.add(name)
    for dep in graph.get(name, []):
        walk(dep)
    ordered.append(name)
for seed in seeds:
    walk(seed)
lines = []
for rel in ordered:
    src = base / rel
    if not src.is_file():
        raise SystemExit("missing module %s" % src)
    dst = dest / Path(rel).name
    dst.write_bytes(src.read_bytes())
    lines.append(dst.name)
(dest / "list").write_text("\n".join(lines) + "\n")
print("modules %d" % len(lines))
ENDMOD
if [ "$CLEAN_MODLOOP" = 1 ]; then
  umount "$MNT" || true
fi
PKG=$WORKDIR/pkgs
rm -rf "$PKG"
mkdir -p "$PKG"
if [ -z "$APK_DIR" ]; then
  APK_DIR=$CACHE/apks
  mkdir -p "$APK_DIR"
  for p in \
    iproute2-minimal-6.9.0-r0.apk \
    libcap2-2.78-r0.apk \
    libmnl-1.0.5-r2.apk \
    libelf-0.191-r0.apk \
    zlib-1.3.2-r0.apk \
    zstd-libs-1.5.6-r0.apk \
    wireguard-tools-wg-1.0.20210914-r4.apk
  do
    if [ ! -s "$APK_DIR/$p" ]; then
      curl -fL --retry 3 -o "$APK_DIR/$p" "$ALPINE_MIRROR/main/x86_64/$p"
    fi
  done
fi
for apk in "$APK_DIR"/*.apk; do
  tar -C "$PKG" -xzf "$apk"
  tar -C "$ROOT" -xzf "$apk"
done
mkdir -p "$SHARE/bin" "$SHARE/musl" "$SHARE/modules" "$SHARE/glibc"
if [ -d "$PKG/bin" ]; then cp -a "$PKG/bin/." "$SHARE/bin/"; fi
if [ -d "$PKG/sbin" ]; then cp -a "$PKG/sbin/." "$SHARE/bin/"; fi
if [ -d "$PKG/usr/bin" ]; then cp -a "$PKG/usr/bin/." "$SHARE/bin/"; fi
if [ -d "$PKG/usr/sbin" ]; then cp -a "$PKG/usr/sbin/." "$SHARE/bin/"; fi
if [ -d "$PKG/lib" ]; then cp -a "$PKG/lib/." "$SHARE/musl/"; fi
if [ -d "$PKG/usr/lib" ]; then cp -a "$PKG/usr/lib/." "$SHARE/musl/"; fi
cp "$MODROOT"/*.ko "$SHARE/modules/"
cp "$MODROOT/list" "$SHARE/modules/list"
cp "$KIT_BIN" "$SHARE/bin/gate4agent-node"
chmod 0755 "$SHARE/bin/gate4agent-node"
python3 - "$KIT_BIN" "$SHARE/glibc" << 'ENDLDD'
import os, shutil, subprocess, sys
from pathlib import Path
binary, dest = sys.argv[1], Path(sys.argv[2])
dest.mkdir(parents=True, exist_ok=True)
out = subprocess.check_output(["ldd", binary], text=True, errors="replace")
copied = []
for line in out.splitlines():
    src = None
    if "=>" in line:
        parts = line.strip().split()
        if len(parts) >= 3 and parts[2].startswith("/"):
            src = parts[2]
    elif line.strip().startswith("/") and "ld-linux" in line:
        src = line.strip().split()[0]
    if not src:
        continue
    shutil.copy(src, dest / os.path.basename(src))
    copied.append(os.path.basename(src))
if not any("ld-linux" in name for name in copied):
    raise SystemExit("ld-linux was not copied")
print("glibc " + " ".join(copied))
ENDLDD
if [ -f "$SHARE/glibc/ld-linux-x86-64.so.2" ]; then
  cp "$SHARE/glibc/ld-linux-x86-64.so.2" "$SHARE/ld-linux-x86-64.so.2"
fi
rm -f "$SHARE/node.env"
if [ -n "$GUEST_ENV_FILE" ]; then
  if [ ! -f "$GUEST_ENV_FILE" ]; then
    echo "GUEST_ENV_FILE is not a file" | tee -a "$CONSOLE_LOG"
    exit 1
  fi
  cp "$GUEST_ENV_FILE" "$SHARE/node.env"
  chmod 0600 "$SHARE/node.env"
fi
if [ -f "$SHARE/node.env" ]; then
  python3 - "$SHARE" << 'ENDKEY'
import os, shutil, sys
from pathlib import Path
share = Path(sys.argv[1])
text = (share / "node.env").read_text()
key = None
for line in text.splitlines():
    line = line.strip()
    if not line or line.startswith("#") or "=" not in line:
        continue
    name, value = line.split("=", 1)
    if name == "GATE4AGENT_WG_PRIVATE_KEY":
        key = value
if not key:
    raise SystemExit(0)
host = Path(key)
if key.startswith("/mnt/kit/"):
    staged = share / key[len("/mnt/kit/"):]
    if host.is_file():
        staged.parent.mkdir(parents=True, exist_ok=True)
        if not staged.exists() or host.resolve() != staged.resolve():
            shutil.copy(host, staged)
        os.chmod(staged, 0o600)
    raise SystemExit(0)
if host.is_file():
    dest = share / "node.key"
    shutil.copy(host, dest)
    os.chmod(dest, 0o600)
    lines = []
    for line in text.splitlines():
        if line.startswith("GATE4AGENT_WG_PRIVATE_KEY="):
            lines.append("GATE4AGENT_WG_PRIVATE_KEY=/mnt/kit/node.key")
        else:
            lines.append(line)
    (share / "node.env").write_text("\n".join(lines) + "\n")
    os.chmod(share / "node.env", 0o600)
ENDKEY
fi
cat > "$SHARE/net.env" << EOF
GUEST_ADDR=$GUEST_ADDR
GUEST_PREFIX=$GUEST_PREFIX
GUEST_GW=$GUEST_GW
EOF
cat > "$SHARE/load-wireguard.sh" << 'ENDLOAD'
#!/bin/sh
set -u
list=/mnt/kit/modules/list
while IFS= read -r name; do
  [ -n "$name" ] || continue
  case "$name" in
    virtio_net.ko|9p.ko|9pnet.ko|9pnet_virtio.ko|netfs.ko|fscache.ko|failover.ko|net_failover.ko) continue ;;
  esac
  if ! insmod "/mnt/kit/modules/$name" 2>/tmp/insmod.err; then
    echo "INSMOD_FAIL $name $(cat /tmp/insmod.err 2>/dev/null)" > /dev/ttyS0
  fi
done < "$list"
if [ -d /sys/module/wireguard ]; then
  echo "WIREGUARD_MODULE=loaded" > /dev/ttyS0
else
  echo "WIREGUARD_MODULE=missing" > /dev/ttyS0
fi
ENDLOAD
chmod 0755 "$SHARE/load-wireguard.sh"
mkdir -p "$ROOT/modules"
cp "$MODROOT"/*.ko "$ROOT/modules/"
cp "$MODROOT/list" "$ROOT/modules/list"
cat > "$ROOT/init" << 'ENDINIT'
#!/bin/sh
export PATH=/bin:/sbin:/usr/bin:/usr/sbin
mount -t proc none /proc
mount -t sysfs none /sys
mount -t devtmpfs none /dev
mkdir -p /dev/pts /dev/shm /run /tmp
mount -t devpts none /dev/pts
mount -t tmpfs none /dev/shm
mount -t tmpfs none /run
mount -t tmpfs none /tmp
echo "INIT_START $(grep MemTotal /proc/meminfo)" > /dev/ttyS0
while IFS= read -r name; do
  [ -n "$name" ] || continue
  case "$name" in
    wireguard.ko|curve25519-x86_64.ko|curve25519-generic.ko|chacha-x86_64.ko|poly1305-x86_64.ko|libchacha.ko|libchacha20poly1305.ko|libcurve25519.ko|libcurve25519-generic.ko|udp_tunnel.ko|ip6_udp_tunnel.ko) continue ;;
  esac
  if ! insmod "/modules/$name" 2>/tmp/insmod.err; then
    echo "BOOT_INSMOD_FAIL $name $(cat /tmp/insmod.err 2>/dev/null)" > /dev/ttyS0
  fi
done < /modules/list
mkdir -p /mnt/kit
if ! mount -t 9p -o trans=virtio,version=9p2000.L,msize=1048576 kit /mnt/kit; then
  echo "KIT_MOUNT_FAIL" > /dev/ttyS0
  dmesg | tail -n 40 > /dev/ttyS0
  exec /bin/sh
fi
echo "KIT_MOUNTED" > /dev/ttyS0
ls /mnt/kit > /dev/ttyS0
mkdir -p /lib64 /lib/x86_64-linux-gnu
if [ -d /mnt/kit/glibc ]; then
  cp -a /mnt/kit/glibc/. /lib/x86_64-linux-gnu/
fi
if [ -f /mnt/kit/ld-linux-x86-64.so.2 ]; then
  cp /mnt/kit/ld-linux-x86-64.so.2 /lib64/ld-linux-x86-64.so.2
fi
if [ -d /mnt/kit/musl ]; then
  cp -a /mnt/kit/musl/. /lib/ 2>/dev/null || true
  mkdir -p /usr/lib
  cp -a /mnt/kit/musl/. /usr/lib/ 2>/dev/null || true
fi
export PATH="/mnt/kit/bin:$PATH"
if [ -x /mnt/kit/load-wireguard.sh ]; then
  /bin/sh /mnt/kit/load-wireguard.sh
fi
if [ -f /mnt/kit/net.env ]; then
  . /mnt/kit/net.env
  ip link set lo up
  ip link set eth0 up
  ip addr add "${GUEST_ADDR}/${GUEST_PREFIX}" dev eth0
  ip route replace default via "$GUEST_GW" dev eth0 || true
  echo "NET_UP addr=$GUEST_ADDR gw=$GUEST_GW" > /dev/ttyS0
  ip addr > /dev/ttyS0
  ip route > /dev/ttyS0
fi
if [ -f /mnt/kit/node.env ]; then
  set -a
  . /mnt/kit/node.env
  set +a
fi
if [ -x /mnt/kit/bin/gate4agent-node ]; then
  echo "NODE_START" > /dev/ttyS0
  mkdir -p /tmp/ws /tmp/state /tmp/home /run/gate4agent
  chmod 0700 /run/gate4agent
  export HOME=/tmp/home
  export XDG_STATE_HOME=${XDG_STATE_HOME:-/tmp/state}
  /mnt/kit/bin/gate4agent-node ${NODE_ARGS:-} > /tmp/node.log 2>&1 &
  echo $! > /tmp/node.pid
  i=0
  attached=0
  while [ "$i" -lt 90 ]; do
    if grep -q "announced this node" /tmp/node.log 2>/dev/null; then
      echo "NODE_ATTACHED" > /dev/ttyS0
      cat /tmp/node.log > /dev/ttyS0
      attached=1
      break
    fi
    if ! kill -0 "$(cat /tmp/node.pid)" 2>/dev/null; then
      echo "NODE_EXITED" > /dev/ttyS0
      cat /tmp/node.log > /dev/ttyS0
      attached=1
      break
    fi
    i=$((i + 1))
    sleep 1
  done
  if [ "$attached" = 0 ]; then
    echo "NODE_WAIT_DONE" > /dev/ttyS0
    cat /tmp/node.log > /dev/ttyS0
  fi
else
  echo "NODE_BINARY_MISSING" > /dev/ttyS0
fi
if [ -x /mnt/kit/start-frames.sh ]; then
  echo "FRAMES_START" > /dev/ttyS0
  /bin/sh /mnt/kit/start-frames.sh > /tmp/frames.log 2>&1 &
fi
echo "GUEST_READY $(grep MemTotal /proc/meminfo)" > /dev/ttyS0
while true; do
  sleep 3600
done
ENDINIT
chmod 0755 "$ROOT/init"
INITRD=$WORKDIR/initrd.gz
python3 - "$ROOT" "$INITRD" << 'ENDCPIO'
import gzip, os, stat, sys
from pathlib import Path
root, outp = Path(sys.argv[1]), sys.argv[2]
def align(n):
    return (4 - (n % 4)) % 4
def header(ino, mode, filesize, namesize, mtime):
    fields = [
        "070701",
        "%08X" % ino,
        "%08X" % mode,
        "%08X" % 0,
        "%08X" % 0,
        "%08X" % 1,
        "%08X" % mtime,
        "%08X" % filesize,
        "%08X" % 0,
        "%08X" % 0,
        "%08X" % 0,
        "%08X" % 0,
        "%08X" % namesize,
        "%08X" % 0,
    ]
    return "".join(fields).encode("ascii")
chunks = []
ino = 1
for dirpath, dirnames, filenames in os.walk(root):
    dirnames.sort()
    filenames.sort()
    entries = [(".", True)] if Path(dirpath) == root else []
    # record the directory itself, then children files; subdirs are visited by walk
    rel_dir = Path(dirpath).relative_to(root)
    name = "." if rel_dir == Path(".") else str(rel_dir).replace("\\", "/")
    mode = stat.S_IFDIR | 0o755
    namesize = len(name) + 1
    chunks.append(header(ino, mode, 0, namesize, 0) + name.encode() + b"\0" + b"\0" * align(110 + namesize))
    ino += 1
    for fn in filenames:
        path = Path(dirpath) / fn
        rel = str(path.relative_to(root)).replace("\\", "/")
        st = path.lstat()
        if stat.S_ISLNK(st.st_mode):
            data = os.readlink(path).encode()
            mode = stat.S_IFLNK | (st.st_mode & 0o777)
        elif stat.S_ISREG(st.st_mode):
            data = path.read_bytes()
            mode = stat.S_IFREG | (st.st_mode & 0o777)
        else:
            continue
        namesize = len(rel) + 1
        blob = header(ino, mode, len(data), namesize, int(st.st_mtime)) + rel.encode() + b"\0"
        blob += b"\0" * align(len(blob))
        blob += data
        blob += b"\0" * align(len(data))
        chunks.append(blob)
        ino += 1
name = "TRAILER!!!"
namesize = len(name) + 1
blob = header(ino, 0, 0, namesize, 0) + name.encode() + b"\0"
blob += b"\0" * align(len(blob))
chunks.append(blob)
raw = b"".join(chunks)
with gzip.open(outp, "wb", compresslevel=1) as fh:
    fh.write(raw)
print("initrd_bytes", os.path.getsize(outp))
ENDCPIO
case "$QEMU_NET" in
  user) NETDEV="-netdev user,id=n0 -device virtio-net-pci,netdev=n0" ;;
  tap) NETDEV="-netdev tap,id=n0,ifname=$TAP_IF,script=no,downscript=no -device virtio-net-pci,netdev=n0" ;;
esac
echo "$GUEST_MEM" > "$WORKDIR/guest-mem-mib.txt"
# Serial goes to a chardev file. -nographic is not used: with no tty it
# swallows the console. KVM is tried first (root, /dev/kvm). If the guest
# writes nothing, this host's kvm vcpu create is broken and the boot is
# restarted on tcg so the console log is real.
: > "$CONSOLE_LOG"
launch() {
  accel=$1
  cpu=$2
  # shellcheck disable=SC2086
  set -- qemu-system-x86_64 \
    -machine pc,accel="$accel" \
    -cpu "$cpu" \
    -m "$GUEST_MEM" \
    -smp 1 \
    -display none \
    -no-reboot \
    -kernel "$QEMU_KERNEL" \
    -initrd "$INITRD" \
    -append "earlyprintk=ttyS0,115200 rdinit=/init console=ttyS0,115200 loglevel=7 net.ifnames=0" \
    -fsdev "local,id=kitfs,path=$SHARE,security_model=none" \
    -device virtio-9p-pci,fsdev=kitfs,mount_tag=kit \
    -chardev "file,id=ser,path=$CONSOLE_LOG" \
    -serial chardev:ser \
    -monitor none \
    $NETDEV
  if [ -n "$QEMU_NETNS" ]; then
    set -- ip netns exec "$QEMU_NETNS" "$@"
  fi
  if [ "$FOREGROUND" = 1 ]; then
    echo "QEMU_GUEST_MEM=${GUEST_MEM}M" 
    echo "QEMU_ACCEL=$accel"
    exec "$@"
  fi
  "$@" &
  echo $! > "$WORKDIR/qemu.pid"
  echo "QEMU_PID=$(cat "$WORKDIR/qemu.pid") accel=$accel mem=${GUEST_MEM}M" 
}
ACCEL=${QEMU_ACCEL:-kvm}
if [ "$ACCEL" = kvm ]; then
  launch kvm host
  i=0
  while [ "$i" -lt 8 ]; do
    if grep -q "Linux version" "$CONSOLE_LOG" 2>/dev/null; then
      break
    fi
    i=$((i + 1))
    sleep 1
  done
  if ! grep -q "Linux version" "$CONSOLE_LOG" 2>/dev/null; then
    echo "KVM produced no console; restarting on tcg" >> "$CONSOLE_LOG"
    kill "$(cat "$WORKDIR/qemu.pid")" 2>/dev/null || true
    sleep 1
    # the pid file may be sudo/ip; stop any qemu still holding the tap
    pkill -f "qemu-system-x86_64.*$SHARE" 2>/dev/null || true
    sleep 1
    : > "$CONSOLE_LOG"
    echo "KVM_FAILED no guest console; accel=tcg" >> "$CONSOLE_LOG"
    launch tcg max
  fi
else
  launch "$ACCEL" max
fi
