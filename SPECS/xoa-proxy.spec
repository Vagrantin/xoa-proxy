Name:    xoa-proxy
Version: %{pkg_version}
Release: %{_release}.g%{_shortcommit}.static
Summary: Community XOA deployment proxy for XCP-ng
License:        AGPL-3.0-only
BuildArch: x86_64

%define _binary_payload w2.xzdio

Source0: xoa-proxy
Source1: xoa-proxy.service
Source2: xoa-proxy.logrotate
Source3: 83-xoa-proxy.preset

Requires: systemd

Source4: LICENSE
Source5: xoa-proxy.socket

%description
Lightweight HTTP proxy that serves the community XVA image for XO Lite deployment.

%prep
# nothing to unpack, pre-built static binary

%build
# binary already compiled by CI

%install
install -D -m 644 %{SOURCE4} %{buildroot}%{_licensedir}/%{name}/LICENSE
install -D -m 755 %{SOURCE0} \
    %{buildroot}/opt/xensource/bin/xoa-proxy
install -D -m 644 %{SOURCE1} \
    %{buildroot}/usr/lib/systemd/system/xoa-proxy.service
install -D -m 644 %{SOURCE2} \
    %{buildroot}/etc/logrotate.d/xoa-proxy
install -D -m 644 %{SOURCE3} \
    %{buildroot}/usr/lib/systemd/system-preset/83-xoa-proxy.preset
install -D -m 644 %{SOURCE5} \
    %{buildroot}/usr/lib/systemd/system/xoa-proxy.socket

%post
# Written out rather than using the %%systemd_* macros: this RPM is built in a
# rockylinux:9 container but targets CentOS 7 dom0. The build image has no
# systemd macros, so they were emitted literally and the shell read
# "%%systemd_post" as a job spec ("fg: no job control"), failing every scriptlet
# and stranding the old package in the rpmdb on upgrade.
systemctl daemon-reload >/dev/null 2>&1 || :
if [ $1 -eq 1 ] ; then
    # Initial install: apply the shipped 83-xoa-proxy.preset.
    systemctl preset xoa-proxy.socket xoa-proxy.service >/dev/null 2>&1 || :
else
    # Migrate the old always-on service. Do not interrupt an import here:
    # the previous RPM's postun try-restart drains it using graceful shutdown,
    # then Requires=xoa-proxy.socket provides the new listener before startup.
    systemctl disable xoa-proxy.service >/dev/null 2>&1 || :
    systemctl enable xoa-proxy.socket >/dev/null 2>&1 || :
fi
# An old active daemon still owns port 9001 until its postun restart.
if ! systemctl is-active --quiet xoa-proxy.service ; then
    systemctl start xoa-proxy.socket >/dev/null 2>&1 || :
fi

%preun
if [ $1 -eq 0 ] ; then
    # Real removal, not an upgrade.
    systemctl --no-reload disable xoa-proxy.service >/dev/null 2>&1 || :
    systemctl --no-reload disable xoa-proxy.socket >/dev/null 2>&1 || :
    systemctl stop xoa-proxy.socket >/dev/null 2>&1 || :
    systemctl stop xoa-proxy.service >/dev/null 2>&1 || :
fi

%postun
systemctl daemon-reload >/dev/null 2>&1 || :
if [ $1 -ge 1 ] ; then
    # Upgrade, not uninstall: restart only if it was already running.
    systemctl try-restart xoa-proxy.service >/dev/null 2>&1 || :
    systemctl start xoa-proxy.socket >/dev/null 2>&1 || :
fi

%files
%license %{_licensedir}/%{name}/LICENSE
/opt/xensource/bin/xoa-proxy
/usr/lib/systemd/system/xoa-proxy.service
/usr/lib/systemd/system/xoa-proxy.socket
%config(noreplace) /etc/logrotate.d/xoa-proxy
/usr/lib/systemd/system-preset/83-xoa-proxy.preset
 

%changelog
* Tue Oct 27 2026 Community Build <community@build> - 0.1.3-1
- Socket activation and safe idle shutdown; reduce runtime worker threads
* Mon May 09 2026 Community Build <community@build> - 0.1.0-2
Added systemd preset to automatically enable xoa-proxy service
* Mon May 08 2026 Community Build <community@build> - 0.1.0-1
- Initial community release
