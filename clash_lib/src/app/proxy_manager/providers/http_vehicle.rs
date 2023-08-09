use super::{ProviderVehicle, ProviderVehicleType};
use crate::app::ThreadSafeDNSResolver;
use crate::common::errors::map_io_error;
use crate::common::http::{new_http_client, HttpClient};

use async_trait::async_trait;

use hyper::{body, Uri};

use std::io;

use std::path::{Path, PathBuf};

pub struct Vehicle {
    pub url: Uri,
    pub path: PathBuf,
    http_client: HttpClient,
}

impl Vehicle {
    pub fn new<T: Into<Uri>, P: AsRef<Path>>(
        url: T,
        path: P,
        dns_resolver: ThreadSafeDNSResolver,
    ) -> Self {
        let client = new_http_client(dns_resolver).expect("failed to create http client");
        Self {
            url: url.into(),
            path: path.as_ref().to_path_buf(),
            http_client: client,
        }
    }
}

#[async_trait]
impl ProviderVehicle for Vehicle {
    async fn read(&self) -> std::io::Result<Vec<u8>> {
        body::to_bytes(
            self.http_client
                .get(self.url.clone())
                .await
                .map_err(|x| io::Error::new(io::ErrorKind::Other, x.to_string()))?,
        )
        .await
        .map_err(map_io_error)
        .map(|x| x.into_iter().collect::<Vec<u8>>())
    }

    fn path(&self) -> &str {
        self.path.to_str().unwrap()
    }

    fn typ(&self) -> ProviderVehicleType {
        ProviderVehicleType::HTTP
    }
}

#[cfg(test)]
mod tests {
    use super::ProviderVehicle;
    use std::str;
    use std::sync::Arc;

    use http::Uri;

    use crate::app::{dns::Resolver, ThreadSafeDNSResolver};

    #[tokio::test]
    async fn test_http_vehicle() {
        let u = "http://mockbin.org/bin/db6924ba-6b95-4766-b926-e609e1ce49d2"
            .parse::<Uri>()
            .unwrap();
        let r = Arc::new(Resolver::new_default().await);
        let v = super::Vehicle::new(
            u,
            "/tmp/test_http_vehicle",
            r.clone() as ThreadSafeDNSResolver,
        );

        let data = v.read().await.unwrap();
        assert_eq!(str::from_utf8(&data).unwrap(), "ok");
    }
}