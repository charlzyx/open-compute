//go:build linux

package main

import (
	"errors"
	"io"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"syscall"
	"unsafe"
)

const directoryQuotaRoot = "/open-compute-rootfs"
const directoryQuotaMounts = "/open-compute-directories"

// Linux fsxattr has the same 28-byte layout on the supported amd64 and arm64 hosts.
type directoryQuotaAttributes struct {
	Flags, ExtentSize, Extents, Project, CowExtentSize uint32
	Padding                                            [8]byte
}

func directoryQuotaIOCTL(file *os.File, write bool, value *directoryQuotaAttributes) error {
	request := uintptr(0x801c581f) // FS_IOC_FSGETXATTR
	if write {
		request = 0x401c5820 // FS_IOC_FSSETXATTR
	}
	_, _, errno := syscall.Syscall(syscall.SYS_IOCTL, file.Fd(), request, uintptr(unsafe.Pointer(value)))
	runtime.KeepAlive(value)
	if errno != 0 {
		return errno
	}
	return nil
}

func openQuotaDirectory(path string) (*os.File, syscall.Stat_t, error) {
	var info syscall.Stat_t
	fd, err := syscall.Open(path, syscall.O_RDONLY|syscall.O_DIRECTORY|syscall.O_NOFOLLOW|syscall.O_CLOEXEC, 0)
	if err != nil {
		return nil, info, err
	}
	file := os.NewFile(uintptr(fd), path)
	var fs syscall.Statfs_t
	if syscall.Fstat(fd, &info) != nil || syscall.Fstatfs(fd, &fs) != nil || fs.Type != 0x58465342 {
		file.Close()
		return nil, info, errors.New("DIRECTORY_QUOTA_UNAVAILABLE")
	}
	return file, info, nil
}

// Run only before the tenant starts, with the rootfs upper directory mounted read-only.
func prepareDirectoryQuota() error {
	root, rootInfo, err := openQuotaDirectory(directoryQuotaRoot)
	if err != nil {
		return err
	}
	defer root.Close()
	var source directoryQuotaAttributes
	if directoryQuotaIOCTL(root, false, &source) != nil || source.Project == 0 || source.Flags&0x200 == 0 {
		return errors.New("DIRECTORY_QUOTA_UNAVAILABLE")
	}
	entries, err := os.ReadDir(directoryQuotaMounts)
	if err != nil || len(entries) == 0 {
		return errors.New("DIRECTORY_QUOTA_UNAVAILABLE")
	}
	for index := range entries {
		path := filepath.Join(directoryQuotaMounts, strconv.Itoa(index))
		if err := inheritDirectoryQuota(path, rootInfo.Dev, source.Project); err != nil {
			return err
		}
	}
	return nil
}

// Docker creates empty placeholder directories for nested mounts before restoration.
func inheritDirectoryQuota(path string, device uint64, project uint32) error {
	pending := []string{path}
	remaining := 100_000
	for len(pending) != 0 {
		if remaining == 0 {
			return errors.New("DIRECTORY_QUOTA_UNAVAILABLE")
		}
		remaining--
		path := pending[len(pending)-1]
		pending = pending[:len(pending)-1]
		err := func() error {
			file, info, err := openQuotaDirectory(path)
			if err != nil {
				return err
			}
			defer file.Close()
			if project == 0 || info.Dev != device || info.Uid != 0 {
				return errors.New("DIRECTORY_QUOTA_UNAVAILABLE")
			}
			children, err := directoryQuotaChildren(file, remaining-len(pending))
			if err != nil {
				return err
			}
			var destination directoryQuotaAttributes
			if directoryQuotaIOCTL(file, false, &destination) != nil || destination.Project != 0 {
				return errors.New("DIRECTORY_QUOTA_UNAVAILABLE")
			}
			destination.Project = project
			destination.Flags |= 0x200
			if err := directoryQuotaIOCTL(file, true, &destination); err != nil {
				return err
			}
			var applied directoryQuotaAttributes
			if directoryQuotaIOCTL(file, false, &applied) != nil || applied.Project != project || applied.Flags&0x200 == 0 {
				return errors.New("DIRECTORY_QUOTA_UNAVAILABLE")
			}
			for _, name := range children {
				pending = append(pending, filepath.Join(path, name))
			}
			return nil
		}()
		if err != nil {
			return err
		}
	}
	return nil
}

func directoryQuotaChildren(file *os.File, maximum int) ([]string, error) {
	if maximum < 0 {
		return nil, errors.New("DIRECTORY_QUOTA_UNAVAILABLE")
	}
	entries, err := file.ReadDir(maximum + 1)
	if (err != nil && !errors.Is(err, io.EOF)) || len(entries) > maximum {
		return nil, errors.New("DIRECTORY_QUOTA_UNAVAILABLE")
	}
	children := make([]string, 0, len(entries))
	for _, entry := range entries {
		if !entry.IsDir() {
			return nil, errors.New("DIRECTORY_QUOTA_UNAVAILABLE")
		}
		children = append(children, entry.Name())
	}
	return children, nil
}
