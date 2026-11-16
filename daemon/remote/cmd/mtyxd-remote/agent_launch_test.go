package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestOmoEnsurePluginInvalidJSONErrorDoesNotExposeUserPath(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)

	userDir := filepath.Join(home, ".config", "opencode")
	if err := os.MkdirAll(userDir, 0755); err != nil {
		t.Fatalf("failed to create user config dir: %v", err)
	}
	userJSONPath := filepath.Join(userDir, "opencode.json")
	if err := os.WriteFile(userJSONPath, []byte("{"), 0644); err != nil {
		t.Fatalf("failed to write invalid config: %v", err)
	}

	err := omoEnsurePlugin(os.Getenv("PATH"))
	if err == nil {
		t.Fatal("omoEnsurePlugin returned nil for invalid opencode.json")
	}

	msg := err.Error()
	if strings.Contains(msg, home) || strings.Contains(msg, userJSONPath) {
		t.Fatalf("error %q exposes user config path %q", msg, userJSONPath)
	}
	if !strings.Contains(msg, "invalid opencode.json") {
		t.Fatalf("error = %q, want generic invalid opencode.json message", msg)
	}
}

func TestEnsureClaudeNodeOptionsRestoreModuleUsesPrivateUserDir(t *testing.T) {
	home := t.TempDir()
	t.Setenv("HOME", home)

	got, err := ensureClaudeNodeOptionsRestoreModule()
	if err != nil {
		t.Fatalf("ensureClaudeNodeOptionsRestoreModule failed: %v", err)
	}
	want := filepath.Join(home, ".mattyxterm", "node-options", "restore-node-options.cjs")
	if got != want {
		t.Fatalf("restore module path = %q, want %q", got, want)
	}

	dirInfo, err := os.Stat(filepath.Dir(got))
	if err != nil {
		t.Fatalf("stat restore module dir: %v", err)
	}
	if perm := dirInfo.Mode().Perm(); perm != 0700 {
		t.Fatalf("restore module dir mode = %04o, want 0700", perm)
	}
}
