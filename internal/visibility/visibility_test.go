// SPDX-License-Identifier: GPL-3.0-or-later

package visibility

import (
	"context"
	"reflect"
	"sync/atomic"
	"testing"
	"time"
)

func TestRunWatcher_ReturnsOnContextCancel(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	ticks := make(chan time.Time)
	checkCalls := atomic.Int32{}
	exitCalls := atomic.Int32{}

	runWatcher(ctx, ticks, 2,
		func() checkResult { checkCalls.Add(1); return checkResult{ok: true} },
		func(string) { exitCalls.Add(1) },
	)

	if got := checkCalls.Load(); got != 0 {
		t.Errorf("checkCalls=%d, want 0", got)
	}
	if got := exitCalls.Load(); got != 0 {
		t.Errorf("exitCalls=%d, want 0", got)
	}
}

func TestRunWatcher_SkipsGraceTicks(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	ticks := make(chan time.Time)
	checkCalls := atomic.Int32{}
	exitCalls := atomic.Int32{}

	done := make(chan struct{})
	go func() {
		runWatcher(ctx, ticks, 3,
			func() checkResult { checkCalls.Add(1); return checkResult{ok: true} },
			func(string) { exitCalls.Add(1) },
		)
		close(done)
	}()

	for i := 0; i < 3; i++ {
		ticks <- time.Now()
	}
	if got := checkCalls.Load(); got != 0 {
		t.Errorf("after %d grace ticks: checkCalls=%d, want 0", 3, got)
	}

	cancel()
	<-done
	if got := exitCalls.Load(); got != 0 {
		t.Errorf("exitCalls=%d, want 0", got)
	}
}

func TestRunWatcher_CallsCheckPastGrace(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	ticks := make(chan time.Time)
	checkFired := make(chan struct{}, 8)
	checkCalls := atomic.Int32{}
	exitCalls := atomic.Int32{}

	done := make(chan struct{})
	go func() {
		runWatcher(ctx, ticks, 2,
			func() checkResult {
				checkCalls.Add(1)
				checkFired <- struct{}{}
				return checkResult{ok: true}
			},
			func(string) { exitCalls.Add(1) },
		)
		close(done)
	}()

	ticks <- time.Now()
	ticks <- time.Now()
	ticks <- time.Now()
	<-checkFired

	if got := checkCalls.Load(); got != 1 {
		t.Errorf("checkCalls=%d, want 1", got)
	}
	if got := exitCalls.Load(); got != 0 {
		t.Errorf("exitCalls=%d, want 0", got)
	}

	cancel()
	<-done
}

func TestRunWatcher_ExitsOnFailedCheck(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	ticks := make(chan time.Time)
	var exitReason string
	exitCalls := atomic.Int32{}

	done := make(chan struct{})
	go func() {
		runWatcher(ctx, ticks, 0,
			func() checkResult { return checkResult{ok: false, reason: "boom"} },
			func(reason string) {
				exitReason = reason
				exitCalls.Add(1)
			},
		)
		close(done)
	}()

	ticks <- time.Now()
	<-done

	if exitReason != "boom" {
		t.Errorf("exitReason=%q, want %q", exitReason, "boom")
	}
	if got := exitCalls.Load(); got != 1 {
		t.Errorf("exitCalls=%d, want 1", got)
	}
}

func TestRunWatcher_NoMoreChecksAfterExit(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	ticks := make(chan time.Time, 4)
	checkCalls := atomic.Int32{}
	exitCalls := atomic.Int32{}

	done := make(chan struct{})
	go func() {
		runWatcher(ctx, ticks, 0,
			func() checkResult {
				checkCalls.Add(1)
				return checkResult{ok: false, reason: "stop"}
			},
			func(string) { exitCalls.Add(1) },
		)
		close(done)
	}()

	ticks <- time.Now()
	ticks <- time.Now()
	ticks <- time.Now()
	<-done

	if got := checkCalls.Load(); got != 1 {
		t.Errorf("checkCalls=%d, want 1 (further ticks must be ignored)", got)
	}
	if got := exitCalls.Load(); got != 1 {
		t.Errorf("exitCalls=%d, want 1", got)
	}
}

func TestBuildAncestors_LinearChain(t *testing.T) {
	parents := map[uint32]uint32{
		100: 200,
		200: 300,
		300: 0,
	}
	got := buildAncestors(100, parents)
	want := map[uint32]struct{}{100: {}, 200: {}, 300: {}}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("got %v, want %v", got, want)
	}
}

func TestBuildAncestors_SelfLoop(t *testing.T) {
	parents := map[uint32]uint32{42: 42}
	got := buildAncestors(42, parents)
	want := map[uint32]struct{}{42: {}}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("got %v, want %v", got, want)
	}
}

func TestBuildAncestors_MissingParent(t *testing.T) {
	parents := map[uint32]uint32{}
	got := buildAncestors(7, parents)
	want := map[uint32]struct{}{7: {}}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("got %v, want %v", got, want)
	}
}

func TestBuildAncestors_StopsAtSystemPid(t *testing.T) {
	parents := map[uint32]uint32{10: 4, 4: 0}
	got := buildAncestors(10, parents)
	want := map[uint32]struct{}{10: {}}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("got %v, want %v (PID 4 is System; walk stops before adding it)", got, want)
	}
}

func TestBuildAncestors_StopsAtZero(t *testing.T) {
	parents := map[uint32]uint32{}
	got := buildAncestors(0, parents)
	want := map[uint32]struct{}{}
	if !reflect.DeepEqual(got, want) {
		t.Errorf("got %v, want %v", got, want)
	}
}

func TestBuildAncestors_DepthCap(t *testing.T) {
	// Use PIDs starting at 1000 to avoid the System (4) and zero stop conditions.
	const base uint32 = 1000
	parents := map[uint32]uint32{}
	for i := uint32(0); i < 100; i++ {
		parents[base+i] = base + i + 1
	}
	got := buildAncestors(base, parents)
	if len(got) != maxAncestorDepth {
		t.Errorf("len(got)=%d, want %d", len(got), maxAncestorDepth)
	}
	for i := uint32(0); i < uint32(maxAncestorDepth); i++ {
		if _, ok := got[base+i]; !ok {
			t.Errorf("ancestor PID %d missing from result", base+i)
		}
	}
}
