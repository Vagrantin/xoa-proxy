Name:    xoa-proxy
Version: 0.1.0
Release: 1%{?dist}
Summary: Community XOA deployment proxy for XCP-ng
License: GPLv3
BuildArch: x86_64

%define _binary_payload w2.xzdio

Source0: xoa-proxy
Source1: xoa-proxy.service
Source2: xoa-proxy.logrotate

Requires: systemd

%description
Lightweight HTTP proxy that serves the community XVA image for XO Lite deployment.

%prep
# nothing to unpack — pre-built static binary

%build
# binary already compiled by CI

%install
install -D -m 755 %{SOURCE0} \
    %{buildroot}/opt/xensource/bin/xoa-proxy
install -D -m 644 %{SOURCE1} \
    %{buildroot}/usr/lib/systemd/system/xoa-proxy.service
install -D -m 644 %{SOURCE2} \
    %{buildroot}/etc/logrotate.d/xoa-proxy

%post
%systemd_post xoa-proxy.service

%preun
%systemd_preun xoa-proxy.service

%postun
%systemd_postun_with_restart xoa-proxy.service

%files
/opt/xensource/bin/xoa-proxy
/usr/lib/systemd/system/xoa-proxy.service
%config(noreplace) /etc/logrotate.d/xoa-proxy

%changelog
* Mon May 08 2026 Community Build <community@build> - 0.1.0-1
- Initial community release
