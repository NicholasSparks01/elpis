/*
   A vminitd plugin for elpis: keeps the guest's clock in step with the host's.

   Service containerd.vminitd.services.clock.v1.Clock, method Resync, as sbx's vminitd has, so elpis-shim
   drives either guest the same way. The request is a google.protobuf.Timestamp: the host's time when the
   shim sent it. Without one (sbx's shim sends an empty request) the time comes from a PTP clock, which a
   virtio-rtc device provides on kernels that have its driver.

   Licensed under the Apache License, Version 2.0.
*/

package clock

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"time"

	"github.com/containerd/log"
	"github.com/containerd/plugin"
	"github.com/containerd/plugin/registry"
	"github.com/containerd/ttrpc"
	"golang.org/x/sys/unix"
	"google.golang.org/protobuf/types/known/emptypb"
	"google.golang.org/protobuf/types/known/timestamppb"

	"github.com/containerd/nerdbox/plugins"
)

const serviceName = "containerd.vminitd.services.clock.v1.Clock"

func init() {
	registry.Register(&plugin.Registration{
		Type: plugins.TTRPCPlugin,
		ID:   "clock",
		InitFn: func(ic *plugin.InitContext) (interface{}, error) {
			s := &service{}
			if err := s.syncFromPTP(); err == nil {
				log.G(ic.Context).Info("clock: boot sync from PTP")
			}
			return s, nil
		},
	})
}

type service struct{}

func (s *service) RegisterTTRPC(server *ttrpc.Server) error {
	server.RegisterService(serviceName, &ttrpc.ServiceDesc{
		Methods: map[string]ttrpc.Method{
			"Resync": func(ctx context.Context, unmarshal func(interface{}) error) (interface{}, error) {
				var host timestamppb.Timestamp
				if err := unmarshal(&host); err != nil {
					return nil, err
				}
				return &emptypb.Empty{}, s.resync(ctx, &host)
			},
		},
	})
	return nil
}

func (s *service) resync(ctx context.Context, host *timestamppb.Timestamp) error {
	if host.GetSeconds() > 0 {
		before := time.Now()
		if err := setRealtime(host.AsTime()); err != nil {
			return err
		}
		log.G(ctx).WithField("offset", host.AsTime().Sub(before)).Info("clock: resynced from host time")
		return nil
	}
	return s.syncFromPTP()
}

// syncFromPTP sets the system clock from the first PTP clock (/dev/ptp*), if there is one.
func (s *service) syncFromPTP() error {
	devices, _ := filepath.Glob("/dev/ptp*")
	if len(devices) == 0 {
		return errors.New("no PTP clock (virtio-rtc) present")
	}
	f, err := os.Open(devices[0])
	if err != nil {
		return err
	}
	defer f.Close()
	// A dynamic POSIX clock id for an open PTP device (FD_TO_CLOCKID in the kernel)
	clock := (^int32(f.Fd()))<<3 | 3
	var ts unix.Timespec
	if err := unix.ClockGettime(clock, &ts); err != nil {
		return err
	}
	return setRealtime(time.Unix(ts.Unix()))
}

func setRealtime(t time.Time) error {
	ts := unix.NsecToTimespec(t.UnixNano())
	return unix.ClockSettime(unix.CLOCK_REALTIME, &ts)
}
