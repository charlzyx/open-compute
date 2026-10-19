//go:build linux

package main

import (
	"errors"
	"io"
	"os"
	"strconv"

	"golang.org/x/sys/unix"
)

type execSupervisorPTY struct{ master, slave *os.File }

func execSupervisorOpenPTY(rows, cols uint16) (*execSupervisorPTY, error) {
	if rows == 0 || cols == 0 {
		return nil, errors.New("INVALID_PTY_SIZE")
	}
	fd, err := unix.Open("/dev/ptmx", unix.O_RDWR|unix.O_NOCTTY|unix.O_CLOEXEC, 0)
	if err != nil {
		return nil, errors.New("PTY_FAILED")
	}
	master := os.NewFile(uintptr(fd), "exec-pty-master")
	fail := func() (*execSupervisorPTY, error) { master.Close(); return nil, errors.New("PTY_FAILED") }
	if unix.IoctlSetPointerInt(fd, unix.TIOCSPTLCK, 0) != nil {
		return fail()
	}
	number, err := unix.IoctlGetInt(fd, unix.TIOCGPTN)
	if err != nil {
		return fail()
	}
	slave, err := os.OpenFile("/dev/pts/"+strconv.Itoa(number), os.O_RDWR|unix.O_NOCTTY, 0)
	if err != nil {
		return fail()
	}
	if unix.IoctlSetWinsize(fd, unix.TIOCSWINSZ, &unix.Winsize{Row: rows, Col: cols}) != nil {
		slave.Close()
		return fail()
	}
	return &execSupervisorPTY{master: master, slave: slave}, nil
}

func (pty *execSupervisorPTY) resize(rows, cols uint16) error {
	if rows == 0 || cols == 0 {
		return errors.New("INVALID_PTY_SIZE")
	}
	if unix.IoctlSetWinsize(int(pty.master.Fd()), unix.TIOCSWINSZ, &unix.Winsize{Row: rows, Col: cols}) != nil {
		return errors.New("PTY_FAILED")
	}
	return nil
}

func (pty *execSupervisorPTY) copyInput(input io.Reader) {
	_, _ = io.Copy(pty.master, input)
	// A PTY has no write-half close. VEOF preserves output in canonical mode.
	termios, err := unix.IoctlGetTermios(int(pty.master.Fd()), unix.TCGETS)
	if err == nil && termios.Lflag&unix.ICANON != 0 && termios.Cc[unix.VEOF] != 0 {
		// The first EOF flushes a partial line; the second ends the following read.
		_, _ = pty.master.Write([]byte{termios.Cc[unix.VEOF], termios.Cc[unix.VEOF]})
	}
}

func (pty *execSupervisorPTY) copyOutput(output io.Writer) error {
	_, err := io.Copy(output, pty.master)
	// Linux reports EIO when the final slave closes.
	if errors.Is(err, unix.EIO) {
		return nil
	}
	return err
}
