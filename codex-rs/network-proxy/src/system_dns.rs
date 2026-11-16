//! macOS native resolution preserves supplemental and VPN split-DNS routes that
//! are not represented by /etc/resolv.conf.
use rama_core::bytes::Bytes;
use rama_core::futures::Stream;
use rama_core::futures::async_stream::stream_fn;
use rama_core::futures::stream;
use rama_dns::client::resolver::DnsAddressResolver;
use rama_dns::client::resolver::DnsResolver;
use rama_dns::client::resolver::DnsTxtResolver;
use rama_net::address::Domain;
use std::io;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use tokio::net::lookup_host;

/// Resolve through macOS's native address resolver without a fallback or an
/// additional timeout. Lookup failures are emitted as stream errors.
#[derive(Clone)]
pub(crate) struct SystemDnsResolver;

impl DnsAddressResolver for SystemDnsResolver {
    type Error = io::Error;

    fn lookup_ipv4(
        &self,
        domain: Domain,
    ) -> impl Stream<Item = Result<Ipv4Addr, Self::Error>> + Send + '_ {
        stream_fn(async move |mut yielder| {
            match lookup_host((domain.as_str(), 0)).await {
                Ok(addresses) => {
                    for address in addresses {
                        if let IpAddr::V4(ip) = address.ip() {
                            yielder.yield_item(Ok(ip)).await;
                        }
                    }
                }
                Err(error) => yielder.yield_item(Err(error)).await,
            }
        })
    }

    fn lookup_ipv6(
        &self,
        domain: Domain,
    ) -> impl Stream<Item = Result<Ipv6Addr, Self::Error>> + Send + '_ {
        stream_fn(async move |mut yielder| {
            match lookup_host((domain.as_str(), 0)).await {
                Ok(addresses) => {
                    for address in addresses {
                        if let IpAddr::V6(ip) = address.ip() {
                            yielder.yield_item(Ok(ip)).await;
                        }
                    }
                }
                Err(error) => yielder.yield_item(Err(error)).await,
            }
        })
    }
}

impl DnsTxtResolver for SystemDnsResolver {
    type Error = io::Error;

    fn lookup_txt(
        &self,
        _domain: Domain,
    ) -> impl Stream<Item = Result<Bytes, Self::Error>> + Send + '_ {
        // TCP connectors only need address lookups. Never fall back to a
        // resolver with different DNS routing for unsupported record types.
        stream::once(std::future::ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the system address resolver does not support TXT lookups",
        ))))
    }
}

impl DnsResolver for SystemDnsResolver {}

#[cfg(test)]
#[path = "system_dns_tests.rs"]
mod tests;
