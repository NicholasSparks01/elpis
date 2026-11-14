// elpis-mkfs: PID 1 of a short-lived "tools VM" that makes filesystems for elpis, so the host needs no
// Windows builds of erofs-utils or e2fsprogs. elpis-shim boots the VM with
//
//	init=/sbin/elpis-mkfs -- erofs           /dev/vdb = a layer tar, /dev/vdc = the output disk
//	init=/sbin/elpis-mkfs -- ext4 LABEL      /dev/vdb = the disk to format
//
// runs the tool, prints "ELPIS-MKFS: OK" or "ELPIS-MKFS: FAILED: …" on the console, and powers off.
package main

import (
	"fmt"
	"os"
	"os/exec"
	"strings"
	"syscall"
)

func main() {
	err := run(os.Args[1:])
	if err != nil {
		report("FAILED: " + strings.ReplaceAll(err.Error(), "\n", " | "))
	} else {
		report("OK")
	}
	syscall.Sync()
	_ = syscall.Reboot(syscall.LINUX_REBOOT_CMD_POWER_OFF)
	select {}
}

func run(args []string) error {
	// The kernel may have mounted it already (CONFIG_DEVTMPFS_MOUNT)
	if err := syscall.Mount("devtmpfs", "/dev", "devtmpfs", 0, ""); err != nil && err != syscall.EBUSY {
		return fmt.Errorf("mounting /dev: %w", err)
	}
	_ = syscall.Mount("proc", "/proc", "proc", 0, "")
	_ = syscall.Mount("tmpfs", "/tmp", "tmpfs", 0, "")
	if len(args) == 0 {
		return fmt.Errorf("no mode given")
	}
	var cmd *exec.Cmd
	switch args[0] {
	case "erofs":
		// --aufs turns OCI ".wh." whiteouts into overlayfs ones
		cmd = exec.Command("/usr/bin/mkfs.erofs", "--tar=f", "--aufs", "--quiet", "-Enoinline_data", "/dev/vdc", "/dev/vdb")
	case "ext4":
		label := "elpis"
		if len(args) > 1 {
			label = args[1]
		}
		// Journaled: an unjournaled layer that loses its VM abruptly can come back corrupt
		cmd = exec.Command("/usr/sbin/mke2fs", "-t", "ext4", "-F", "-q", "-j", "-E", "nodiscard", "-L", label, "/dev/vdb")
	default:
		return fmt.Errorf("unknown mode %q", args[0])
	}
	cmd.Env = []string{"PATH=/usr/sbin:/usr/bin:/sbin:/bin"}
	out, err := cmd.CombinedOutput()
	if err != nil {
		return fmt.Errorf("%s: %v: %s", cmd.Path, err, strings.TrimSpace(string(out)))
	}
	return nil
}

func report(status string) {
	line := "ELPIS-MKFS: " + status + "\n"
	if f, err := os.OpenFile("/dev/hvc0", os.O_WRONLY, 0); err == nil {
		f.WriteString(line)
		f.Close()
	}
	os.Stdout.WriteString(line)
}
