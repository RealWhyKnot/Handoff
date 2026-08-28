// SPDX-License-Identifier: GPL-3.0-or-later

package dispatch

// Failure is a command failure that carries process detail. Handlers that shell
// out return this instead of a success payload with an "ok": false field inside
// it, so the envelope's OK flag means what it says.
type Failure struct {
	Message  string
	ExitCode *int
	Stdout   string
	Stderr   string
}

func (f *Failure) Error() string { return f.Message }

func (f *Failure) Detail() map[string]interface{} {
	d := map[string]interface{}{}
	if f.ExitCode != nil {
		d["exit_code"] = *f.ExitCode
	}
	if f.Stdout != "" {
		d["stdout"] = f.Stdout
	}
	if f.Stderr != "" {
		d["stderr"] = f.Stderr
	}
	return d
}
