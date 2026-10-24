//go:build linux

package main

import (
	"os"
	"path/filepath"
	"testing"
	"unsafe"
)

func TestDirectoryQuotaRejectsMissingAndSymlinkDirectories(t *testing.T) {
	if unsafe.Sizeof(directoryQuotaAttributes{}) != 28 || unsafe.Offsetof(directoryQuotaAttributes{}.Project) != 12 {
		t.Fatal("Linux fsxattr ABI changed")
	}
	parent := t.TempDir()
	file := filepath.Join(parent, "file")
	if os.WriteFile(file, []byte("preserved"), 0o600) != nil || os.Symlink(parent, filepath.Join(parent, "link")) != nil {
		t.Fatal("fixture creation failed")
	}
	for _, path := range []string{filepath.Join(parent, "missing"), file, filepath.Join(parent, "link")} {
		if err := inheritDirectoryQuota(path, 0, 1); err == nil {
			t.Fatal("quota admission accepted a non-directory or symlink")
		}
	}
	if err := inheritDirectoryQuota(parent, 0, 0); err == nil {
		t.Fatal("quota admission accepted an unassigned project")
	}
	if value, err := os.ReadFile(file); err != nil || string(value) != "preserved" {
		t.Fatal("rejected admission changed an existing file")
	}
}

func TestDirectoryQuotaChildrenAcceptsOnlyBoundedDirectorySkeletons(t *testing.T) {
	root := t.TempDir()
	read := func(maximum int) ([]string, error) {
		file, err := os.Open(root)
		if err != nil {
			t.Fatal(err)
		}
		defer file.Close()
		return directoryQuotaChildren(file, maximum)
	}
	if names, err := read(0); err != nil || len(names) != 0 {
		t.Fatal("empty volume was rejected")
	}
	if err := os.Mkdir(filepath.Join(root, "child"), 0o755); err != nil {
		t.Fatal(err)
	}
	if names, err := read(1); err != nil || len(names) != 1 || names[0] != "child" {
		t.Fatal("Docker nested mount placeholder was rejected")
	}
	if _, err := read(0); err == nil {
		t.Fatal("directory node budget was ignored")
	}
	for _, name := range []string{"file", "link"} {
		path := filepath.Join(root, name)
		var err error
		if name == "file" {
			err = os.WriteFile(path, []byte("preserved"), 0o600)
		} else {
			err = os.Symlink("child", path)
		}
		if err != nil {
			t.Fatal(err)
		}
		if _, err := read(4); err == nil {
			t.Fatal("quota admission accepted existing file data or a symlink")
		}
		if err := os.Remove(path); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := read(-1); err == nil {
		t.Fatal("negative directory budget was accepted")
	}
}
