// Copyright 2015-2017 Benjamin Fry <benjaminfry@me.com>
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

//! Lookup result from a resolution of ipv4 and ipv6 records with a Resolver.

use std::cmp::min;
use std::error::Error as StdError;
use std::mem;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::slice::Iter;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{future, task, Async, Future, Poll};

use trust_dns_proto::op::Query;
use trust_dns_proto::rr::rdata;
use trust_dns_proto::rr::{Name, RData, RecordType};
use trust_dns_proto::xfer::{DnsRequest, DnsRequestOptions, DnsResponse};
#[cfg(feature = "dnssec")]
use trust_dns_proto::SecureDnsHandle;
use trust_dns_proto::{DnsHandle, RetryDnsHandle};

use dns_lru::MAX_TTL;
use error::*;
use lookup_ip::LookupIpIter;
use lookup_state::CachingClient;
use name_server_pool::{ConnectionProvider, NameServerPool, StandardConnection};
use resolver_future::BasicResolverHandle;

/// Result of a DNS query when querying for any record type supported by the TRust-DNS Proto library.
///
/// For IP resolution see LookupIp, as it has more features for A and AAAA lookups.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Lookup {
    rdatas: Arc<Vec<RData>>,
    valid_until: Instant,
}

impl Lookup {
    /// Return new instance with given rdatas and the maximum TTL.
    pub fn new_with_max_ttl(rdatas: Arc<Vec<RData>>) -> Self {
        let valid_until = Instant::now() + Duration::from_secs(MAX_TTL as u64);
        Lookup {
            rdatas,
            valid_until,
        }
    }

    /// Return a new instance with the given rdatas and deadline.
    pub fn new_with_deadline(rdatas: Arc<Vec<RData>>, valid_until: Instant) -> Self {
        Lookup {
            rdatas,
            valid_until,
        }
    }

    /// Returns a borrowed iterator of the returned IPs
    pub fn iter(&self) -> LookupIter {
        LookupIter(self.rdatas.iter())
    }

    /// Returns the `Instant` at which this `Lookup` is no longer valid.
    pub fn valid_until(&self) -> Instant {
        self.valid_until
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rdatas.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.rdatas.len()
    }

    /// Clones the inner vec, appends the other vec
    pub(crate) fn append(&self, other: Lookup) -> Self {
        let mut rdatas = Vec::with_capacity(self.len() + other.len());
        rdatas.extend_from_slice(&*self.rdatas);
        rdatas.extend_from_slice(&*other.rdatas);

        // Choose the sooner deadline of the two lookups.
        let valid_until = min(self.valid_until(), other.valid_until());
        Self::new_with_deadline(Arc::new(rdatas), valid_until)
    }
}

impl From<RData> for Lookup {
    fn from(data: RData) -> Self {
        Lookup::new_with_max_ttl(Arc::new(vec![data]))
    }
}

/// Borrowed view of set of RDatas returned from a Lookup
pub struct LookupIter<'a>(Iter<'a, RData>);

impl<'a> Iterator for LookupIter<'a> {
    type Item = &'a RData;

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next()
    }
}

/// Different lookup options for the lookup attempts and validation
#[derive(Clone)]
#[doc(hidden)]
pub enum LookupEither<
    C: DnsHandle<Error = ResolveError> + 'static,
    P: ConnectionProvider<ConnHandle = C> + 'static,
> {
    Retry(RetryDnsHandle<NameServerPool<C, P>>),
    #[cfg(feature = "dnssec")]
    Secure(SecureDnsHandle<RetryDnsHandle<NameServerPool<C, P>>>),
}

impl<C: DnsHandle<Error = ResolveError>, P: ConnectionProvider<ConnHandle = C>> DnsHandle
    for LookupEither<C, P>
{
    type Error = ResolveError;

    fn is_verifying_dnssec(&self) -> bool {
        match *self {
            LookupEither::Retry(ref c) => c.is_verifying_dnssec(),
            #[cfg(feature = "dnssec")]
            LookupEither::Secure(ref c) => c.is_verifying_dnssec(),
        }
    }

    fn send<R: Into<DnsRequest>>(
        &mut self,
        request: R,
    ) -> Box<Future<Item = DnsResponse, Error = Self::Error> + Send> {
        match *self {
            LookupEither::Retry(ref mut c) => c.send(request),
            #[cfg(feature = "dnssec")]
            LookupEither::Secure(ref mut c) => c.send(request),
        }
    }
}

/// The Future returned from ResolverFuture when performing a lookup.
#[doc(hidden)]
pub struct LookupFuture<C = LookupEither<BasicResolverHandle, StandardConnection>>
where
    C: DnsHandle<Error = ResolveError> + 'static,
{
    client_cache: CachingClient<C>,
    names: Vec<Name>,
    record_type: RecordType,
    options: DnsRequestOptions,
    future: Box<Future<Item = Lookup, Error = ResolveError> + Send>,
}

impl<C: DnsHandle<Error = ResolveError> + 'static> LookupFuture<C> {
    /// Perform a lookup from a name and type to a set of RDatas
    ///
    /// # Arguments
    ///
    /// * `names` - a set of DNS names to attempt to resolve, they will be attempted in queue order, i.e. the first is `names.pop()`. Upon each failure, the next will be attempted.
    /// * `record_type` - type of record being sought
    /// * `client_cache` - cache with a connection to use for performing all lookups
    #[doc(hidden)]
    pub fn lookup(
        mut names: Vec<Name>,
        record_type: RecordType,
        options: DnsRequestOptions,
        mut client_cache: CachingClient<C>,
    ) -> Self {
        let name = names.pop().ok_or_else(|| {
            ResolveError::from(ResolveErrorKind::Message("can not lookup for no names"))
        });

        let query: Box<Future<Item = Lookup, Error = ResolveError> + Send> = match name {
            Ok(name) => {
                Box::new(client_cache.lookup(Query::query(name, record_type), options.clone()))
            }
            Err(err) => Box::new(future::err(err)),
        };

        LookupFuture {
            client_cache: client_cache,
            names,
            record_type,
            options,
            future: query,
        }
    }

    fn next_lookup<F: FnOnce() -> Poll<Lookup, ResolveError>>(
        &mut self,
        otherwise: F,
    ) -> Poll<Lookup, ResolveError> {
        let name = self.names.pop();
        if let Some(name) = name {
            let query = self.client_cache
                .lookup(Query::query(name, self.record_type), self.options.clone());

            mem::replace(&mut self.future, Box::new(query));
            // guarantee that we get scheduled for the next turn...
            task::current().notify();
            Ok(Async::NotReady)
        } else {
            otherwise()
        }
    }

    pub(crate) fn error<E: StdError>(client_cache: CachingClient<C>, error: E) -> Self {
        return LookupFuture {
            // errors on names don't need to be cheap... i.e. this clone is unfortunate in this case.
            client_cache,
            names: vec![],
            record_type: RecordType::NULL,
            options: DnsRequestOptions::default(),
            future: Box::new(future::err(
                ResolveErrorKind::Msg(format!("{}", error)).into(),
            )),
        };
    }
}

impl<C: DnsHandle<Error = ResolveError> + 'static> Future for LookupFuture<C> {
    type Item = Lookup;
    type Error = ResolveError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.future.poll() {
            Ok(Async::Ready(lookup_ip)) => if lookup_ip.rdatas.len() == 0 {
                return self.next_lookup(|| Ok(Async::Ready(lookup_ip)));
            } else {
                return Ok(Async::Ready(lookup_ip));
            },
            p @ Ok(Async::NotReady) => p,
            e @ Err(_) => {
                return self.next_lookup(|| e);
            }
        }
    }
}

/// The result of an SRV lookup
#[derive(Debug, Clone)]
pub struct SrvLookup(Lookup);

impl SrvLookup {
    /// Returns an iterator over the SRV RData
    pub fn iter(&self) -> SrvLookupIter {
        SrvLookupIter(self.0.iter())
    }

    /// Returns the list of IPs associated with the SRV record.
    ///
    /// *Note*: the lack of any IPs does not necessarily meant that there are no IPs available for the service, only that they were not included in the original request. A subsequent query for the IPs via the `srv.target()` should resolve to the IPs.
    pub fn ip_iter(&self) -> LookupIpIter {
        LookupIpIter(self.0.iter())
    }
}

impl From<Lookup> for SrvLookup {
    fn from(lookup: Lookup) -> Self {
        SrvLookup(lookup)
    }
}

/// An iterator over the Lookup type
pub struct SrvLookupIter<'i>(LookupIter<'i>);

impl<'i> Iterator for SrvLookupIter<'i> {
    type Item = &'i rdata::SRV;

    fn next(&mut self) -> Option<Self::Item> {
        let iter: &mut _ = &mut self.0;
        iter.filter_map(|rdata| match *rdata {
            RData::SRV(ref data) => Some(data),
            _ => None,
        }).next()
    }
}

/// A Future while resolves to the Lookup type
pub struct SrvLookupFuture(LookupFuture);

impl From<LookupFuture> for SrvLookupFuture {
    fn from(lookup_future: LookupFuture) -> Self {
        SrvLookupFuture(lookup_future)
    }
}

impl Future for SrvLookupFuture {
    type Item = SrvLookup;
    type Error = ResolveError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.0.poll() {
            Ok(Async::Ready(lookup)) => Ok(Async::Ready(SrvLookup(lookup))),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(e) => Err(e),
        }
    }
}

/// Creates a Lookup result type from the specified components
macro_rules! lookup_type {
    ($l:ident, $i:ident, $f:ident, $r:path, $t:path) => {
        /// Contains the results of a lookup for the associated RecordType
        #[derive(Debug, Clone)]
        pub struct $l(Lookup);

        impl $l {
            /// Returns an iterator over the RData
            pub fn iter(&self) -> $i {
                $i(self.0.iter())
            }
        }

        impl From<Lookup> for $l {
            fn from(lookup: Lookup) -> Self {
                $l(lookup)
            }
        }

        /// An iterator over the Lookup type
        pub struct $i<'i>(LookupIter<'i>);

        impl<'i> Iterator for $i<'i> {
            type Item = &'i $t;

            fn next(&mut self) -> Option<Self::Item> {
                let iter: &mut _ = &mut self.0;
                iter.filter_map(|rdata| match *rdata {
                    $r(ref data) => Some(data),
                    _ => None,
                }).next()
            }
        }

        /// A Future while resolves to the Lookup type
        pub struct $f(LookupFuture);

        impl From<LookupFuture> for $f {
            fn from(lookup_future: LookupFuture) -> Self {
                $f(lookup_future)
            }
        }

        impl Future for $f {
            type Item = $l;
            type Error = ResolveError;

            fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
                match self.0.poll() {
                    Ok(Async::Ready(lookup)) => Ok(Async::Ready($l(lookup))),
                    Ok(Async::NotReady) => Ok(Async::NotReady),
                    Err(e) => Err(e),
                }
            }
        }
    };
}

// Generate all Lookup record types
lookup_type!(
    ReverseLookup,
    ReverseLookupIter,
    ReverseLookupFuture,
    RData::PTR,
    Name
);
lookup_type!(
    Ipv4Lookup,
    Ipv4LookupIter,
    Ipv4LookupFuture,
    RData::A,
    Ipv4Addr
);
lookup_type!(
    Ipv6Lookup,
    Ipv6LookupIter,
    Ipv6LookupFuture,
    RData::AAAA,
    Ipv6Addr
);
lookup_type!(MxLookup, MxLookupIter, MxLookupFuture, RData::MX, rdata::MX);
lookup_type!(
    TxtLookup,
    TxtLookupIter,
    TxtLookupFuture,
    RData::TXT,
    rdata::TXT
);

#[cfg(test)]
pub mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::{Arc, Mutex};

    use futures::{future, Future};

    use trust_dns_proto::op::Message;
    use trust_dns_proto::rr::{Name, RData, Record, RecordType};
    use trust_dns_proto::xfer::{DnsRequest, DnsRequestOptions};

    use super::*;

    #[derive(Clone)]
    pub struct MockDnsHandle {
        messages: Arc<Mutex<Vec<ResolveResult<DnsResponse>>>>,
    }

    impl DnsHandle for MockDnsHandle {
        type Error = ResolveError;

        fn send<R: Into<DnsRequest>>(
            &mut self,
            _: R,
        ) -> Box<Future<Item = DnsResponse, Error = Self::Error> + Send> {
            Box::new(future::result(
                self.messages.lock().unwrap().pop().unwrap_or(empty()),
            ))
        }
    }

    pub fn v4_message() -> ResolveResult<DnsResponse> {
        let mut message = Message::new();
        message.insert_answers(vec![Record::from_rdata(
            Name::root(),
            86400,
            RecordType::A,
            RData::A(Ipv4Addr::new(127, 0, 0, 1)),
        )]);
        Ok(message.into())
    }

    pub fn empty() -> ResolveResult<DnsResponse> {
        Ok(Message::new().into())
    }

    pub fn error() -> ResolveResult<DnsResponse> {
        Err(ResolveErrorKind::Io.into())
    }

    pub fn mock(messages: Vec<ResolveResult<DnsResponse>>) -> MockDnsHandle {
        MockDnsHandle {
            messages: Arc::new(Mutex::new(messages)),
        }
    }

    #[test]
    fn test_lookup() {
        assert_eq!(
            LookupFuture::lookup(
                vec![Name::root()],
                RecordType::A,
                DnsRequestOptions::default(),
                CachingClient::new(0, mock(vec![v4_message()])),
            ).wait()
                .unwrap()
                .iter()
                .map(|r| r.to_ip_addr().unwrap())
                .collect::<Vec<IpAddr>>(),
            vec![Ipv4Addr::new(127, 0, 0, 1)]
        );
    }

    #[test]
    fn test_error() {
        assert!(
            LookupFuture::lookup(
                vec![Name::root()],
                RecordType::A,
                DnsRequestOptions::default(),
                CachingClient::new(0, mock(vec![error()])),
            ).wait()
                .is_err()
        );
    }

    #[test]
    fn test_empty_no_response() {
        assert_eq!(
            LookupFuture::lookup(
                vec![Name::root()],
                RecordType::A,
                DnsRequestOptions::default(),
                CachingClient::new(0, mock(vec![empty()])),
            ).wait()
                .unwrap_err()
                .kind(),
            &ResolveErrorKind::NoRecordsFound(Query::query(Name::root(), RecordType::A))
        );
    }
}
