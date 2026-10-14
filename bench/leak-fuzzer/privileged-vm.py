#!/usr/bin/env python3
"""Run a command in a disposable BPF-LSM VM; never reboot or alter the host LSMs.

Requires root, qemu-system-x86_64, busybox-static, cpio, zstd, kmod, iproute2, and an installed
BPF/BTF-enabled generic kernel with 9p modules. The host runtime is read-only;
only the specified repository is writable. The guest has no external network.
"""
import argparse
import gzip
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--kernel', help='installed kernel release (default: newest installed generic image)')
    parser.add_argument('--repo', default=str(Path.cwd()))
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if not args.command:
        parser.error('command required')
    if os.geteuid() != 0:
        parser.error('run as root (the guest mounts a read-only view of the host runtime)')
    repo = Path(args.repo).resolve()
    if not args.kernel:
        images = list(Path('/boot').glob('vmlinuz-*-generic'))
        if not images:
            parser.error('install a generic BPF/BTF kernel or specify --kernel')
        args.kernel = max(images, key=lambda image: image.stat().st_mtime).name.removeprefix('vmlinuz-')
    with tempfile.TemporaryDirectory(prefix='marsh-vm-') as directory:
        root = Path(directory) / 'root'
        root.mkdir()
        for name in ('bin', 'dev', 'proc', 'sys', 'host', 'tmp'):
            (root / name).mkdir()
        shutil.copyfile(shutil.which('busybox'), root / 'bin/busybox')
        os.chmod(root / 'bin/busybox', 0o755)
        for name in ('sh', 'mount', 'modprobe', 'chroot', 'mkdir', 'poweroff', 'echo', 'cat'):
            (root / 'bin' / name).symlink_to('busybox')
        copied = set()

        def module(name):
            if name in copied:
                return
            copied.add(name)
            def info(field):
                return subprocess.check_output(['modinfo', '-k', args.kernel, '-F', field, name], text=True).strip()
            for dependency in info('depends').split(','):
                if dependency:
                    module(dependency)
            filename = info('filename')
            if filename == '(builtin)':
                return
            path = Path(filename)
            target = root / str(path).lstrip('/')
            target.parent.mkdir(parents=True, exist_ok=True)
            if path.suffix == '.zst':
                target = target.with_suffix('')
                with target.open('wb') as output:
                    subprocess.run(['zstd', '-dc', str(path)], stdout=output, check=True)
            else:
                shutil.copyfile(path, target)

        for name in ('9p', '9pnet_virtio', 'virtio_pci'):
            module(name)
        module_dir = root / 'lib/modules' / args.kernel
        module_dir.mkdir(parents=True, exist_ok=True)
        for name in ('modules.order', 'modules.builtin', 'modules.builtin.modinfo'):
            source = Path('/lib/modules') / args.kernel / name
            if source.exists():
                shutil.copyfile(source, module_dir / name)
        subprocess.run(['depmod', '-b', str(root), args.kernel], check=True)
        runtime_path = '/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:' + os.environ.get('PATH', '')
        command = 'export PATH=' + shlex.quote(runtime_path) + '; ip link set lo up && cd ' + shlex.quote(str(repo)) + ' && ' + shlex.join(args.command)
        init = f'''#!/bin/sh
export PATH=/bin
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev
modprobe virtio_pci
modprobe 9pnet_virtio
modprobe 9p
mount -t 9p -o trans=virtio,version=9p2000.L,ro host /host || {{ echo MARSH_VM_FAILURE; poweroff -f; }}
mount -t 9p -o trans=virtio,version=9p2000.L repo {shlex.quote('/host' + str(repo))} || {{ echo MARSH_VM_FAILURE; poweroff -f; }}
mount -t proc proc /host/proc
mount -t sysfs sys /host/sys
mount -t devtmpfs dev /host/dev
mount -t tmpfs tmp /host/tmp
mount -t securityfs securityfs /host/sys/kernel/security
cat /host/sys/kernel/security/lsm
chroot /host /bin/sh -c {shlex.quote(command)}
rc=$?
echo MARSH_VM_EXIT=$rc
poweroff -f
'''
        (root / 'init').write_text(init)
        os.chmod(root / 'init', 0o755)
        archive = Path(directory) / 'initramfs.cpio.gz'
        names = [b'.'] + [os.fsencode(str(path.relative_to(root))) for path in root.rglob('*')]
        result = subprocess.run(['cpio', '-o', '-H', 'newc', '--null', '--quiet'], cwd=root, input=b'\0'.join(names) + b'\0', stdout=subprocess.PIPE, check=True)
        with gzip.open(archive, 'wb') as output:
            output.write(result.stdout)
        qemu = ['qemu-system-x86_64', '-m', '3072', '-smp', '2', '-nographic', '-no-reboot', '-nic', 'none', '-kernel', '/boot/vmlinuz-' + args.kernel, '-initrd', str(archive), '-append', 'console=ttyS0 quiet panic=-1 lsm=capability,landlock,yama,bpf rdinit=/init', '-virtfs', 'local,path=/,mount_tag=host,security_model=none,readonly=on', '-virtfs', 'local,path=' + str(repo) + ',mount_tag=repo,security_model=none']
        if Path('/dev/kvm').exists():
            qemu += ['-enable-kvm', '-cpu', 'host']
        else:
            qemu += ['-cpu', 'max']
        process = subprocess.Popen(qemu, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        exit_code = None
        try:
            for line in process.stdout:
                print(line, end='', flush=True)
                if line.startswith('MARSH_VM_EXIT='):
                    exit_code = int(line.partition('=')[2].strip())
            process.wait()
        finally:
            if process.poll() is None:
                process.terminate()
                process.wait()
        return exit_code if exit_code is not None else 1


if __name__ == '__main__':
    raise SystemExit(main())
