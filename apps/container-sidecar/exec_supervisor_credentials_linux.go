//go:build linux

package main

import (
	"errors"
	"os"
	"os/exec"
	"os/user"
	"runtime"
	"strconv"
	"strings"
	"syscall"

	"golang.org/x/sys/unix"
)

// The Broker supplies the image user or a validated public numeric UID:GID.
func containerCredentials(value string) (*syscall.Credential, error) {
	uidPart, groupPart, explicitGroup := strings.Cut(value, ":")
	if uidPart == "" {
		uidPart = "0"
	}
	if strings.ContainsRune(value, 0) || strings.Contains(groupPart, ":") {
		return nil, errors.New("INVALID_CONTAINER_USER")
	}
	uid, numeric, err := containerNumericID(uidPart)
	if err != nil {
		return nil, err
	}
	var account *user.User
	if numeric {
		account, err = user.LookupId(strconv.FormatUint(uint64(uid), 10))
		if err != nil {
			var absent user.UnknownUserIdError
			if !errors.As(err, &absent) && !errors.Is(err, os.ErrNotExist) {
				return nil, errors.New("INVALID_CONTAINER_USER")
			}
		}
	} else {
		account, err = user.Lookup(uidPart)
		if err != nil {
			return nil, errors.New("INVALID_CONTAINER_USER")
		}
		uid, numeric, err = containerNumericID(account.Uid)
		if err != nil || !numeric {
			return nil, errors.New("INVALID_CONTAINER_USER")
		}
	}
	credential := &syscall.Credential{Uid: uid, Groups: []uint32{}}
	if explicitGroup && groupPart != "" {
		gid, numeric, err := containerNumericID(groupPart)
		if err != nil {
			return nil, err
		}
		if !numeric {
			group, err := user.LookupGroup(groupPart)
			if err != nil {
				return nil, errors.New("INVALID_CONTAINER_USER")
			}
			gid, numeric, err = containerNumericID(group.Gid)
			if err != nil || !numeric {
				return nil, errors.New("INVALID_CONTAINER_USER")
			}
		}
		credential.Gid = gid
	} else if account != nil {
		gid, numeric, err := containerNumericID(account.Gid)
		if err != nil || !numeric {
			return nil, errors.New("INVALID_CONTAINER_USER")
		}
		credential.Gid = gid
		groups, err := account.GroupIds()
		if err != nil {
			return nil, errors.New("INVALID_CONTAINER_USER")
		}
		for _, group := range groups {
			gid, numeric, err := containerNumericID(group)
			if err != nil || !numeric {
				return nil, errors.New("INVALID_CONTAINER_USER")
			}
			credential.Groups = append(credential.Groups, gid)
		}
	}
	return credential, nil
}

func containerNumericID(value string) (uint32, bool, error) {
	id, err := strconv.ParseInt(value, 10, 32)
	if errors.Is(err, strconv.ErrSyntax) {
		return 0, false, nil
	}
	if err != nil || id < 0 {
		return 0, true, errors.New("INVALID_CONTAINER_USER")
	}
	return uint32(id), true, nil
}

// Retain only capabilities already present in the root helper's permitted and bounding sets.
func containerCapabilities() ([2]unix.CapUserData, []uintptr, error) {
	var caps [2]unix.CapUserData
	header := unix.CapUserHeader{Version: unix.LINUX_CAPABILITY_VERSION_3}
	if os.Geteuid() != 0 || unix.Capget(&header, &caps[0]) != nil {
		return caps, nil, errors.New("CONTAINER_CAPABILITIES_UNAVAILABLE")
	}
	var ambient []uintptr
	for bit := uintptr(0); bit < 64; bit++ {
		if caps[bit/32].Permitted&(uint32(1)<<(bit%32)) != 0 {
			bound, err := unix.PrctlRetInt(unix.PR_CAPBSET_READ, bit, 0, 0, 0)
			if err != nil || bound != 1 {
				return caps, nil, errors.New("CONTAINER_CAPABILITIES_UNAVAILABLE")
			}
			ambient = append(ambient, bit)
		}
	}
	return caps, ambient, nil
}

func containerMain(value string, arguments []string) int {
	if len(arguments) == 0 || containerQuotaFilter() != nil {
		return 125
	}
	credential, err := containerCredentials(value)
	if err != nil {
		return 125
	}
	caps, ambient, err := containerCapabilities()
	if err != nil {
		return 125
	}
	path, err := exec.LookPath(arguments[0])
	if err != nil {
		return 125
	}
	runtime.LockOSThread()
	groups := make([]int, len(credential.Groups))
	for index, group := range credential.Groups {
		groups[index] = int(group)
	}
	if unix.Prctl(unix.PR_SET_KEEPCAPS, 1, 0, 0, 0) != nil || syscall.Setgroups(groups) != nil ||
		syscall.Setresgid(int(credential.Gid), int(credential.Gid), int(credential.Gid)) != nil ||
		syscall.Setresuid(int(credential.Uid), int(credential.Uid), int(credential.Uid)) != nil {
		return 125
	}
	for index := range caps {
		caps[index].Effective, caps[index].Inheritable = caps[index].Permitted, caps[index].Permitted
	}
	header := unix.CapUserHeader{Version: unix.LINUX_CAPABILITY_VERSION_3}
	if unix.Capset(&header, &caps[0]) != nil {
		return 125
	}
	for _, bit := range ambient {
		if unix.Prctl(unix.PR_CAP_AMBIENT, unix.PR_CAP_AMBIENT_RAISE, bit, 0, 0) != nil {
			return 125
		}
	}
	_ = syscall.Exec(path, arguments, os.Environ())
	return 125
}

// A root-level read-only directory cannot be rebound by renaming a writable ancestor.
func containerHelperImmutable(directory string) bool {
	fd, err := unix.Open(directory, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
	if err != nil {
		return false
	}
	defer unix.Close(fd)
	var filesystem unix.Statfs_t
	if unix.Fstatfs(fd, &filesystem) != nil || filesystem.Flags&unix.ST_RDONLY == 0 {
		return false
	}
	file, err := unix.Openat(fd, "helper", unix.O_RDONLY|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0)
	if err != nil {
		return false
	}
	defer unix.Close(file)
	var info unix.Stat_t
	return unix.Fstat(file, &info) == nil && info.Mode&unix.S_IFMT == unix.S_IFREG &&
		unix.Fstatfs(file, &filesystem) == nil && filesystem.Flags&unix.ST_RDONLY != 0
}
