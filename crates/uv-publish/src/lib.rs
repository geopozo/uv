use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use distribution_filename::{DistFilename, SourceDistExtension, SourceDistFilename};
use fs_err::File;
use futures::TryStreamExt;
use glob::{glob, GlobError, PatternError};
use itertools::Itertools;
use pypi_types::{Metadata23, MetadataError};
use reqwest::header::AUTHORIZATION;
use reqwest::multipart::Part;
use reqwest::{Body, Response, StatusCode};
use reqwest_middleware::RequestBuilder;
use rustc_hash::FxHashSet;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{fmt, io};
use thiserror::Error;
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;
use tracing::{debug, enabled, trace, Level};
use url::Url;
use uv_client::BaseClient;
use uv_fs::{ProgressReader, Simplified};
use uv_metadata::read_metadata_async_seek;

#[derive(Error, Debug)]
pub enum PublishError {
    #[error("Invalid publish path: `{0}`")]
    Pattern(String, #[source] PatternError),
    /// [`GlobError`] is a wrapped io error.
    #[error(transparent)]
    Glob(#[from] GlobError),
    #[error("Path patterns didn't match any wheels or source distributions")]
    NoFiles,
    #[error(transparent)]
    Fmt(#[from] fmt::Error),
    #[error("File is neither a wheel nor a source distribution: `{}`", _0.user_display())]
    InvalidFilename(PathBuf),
    #[error("Failed to publish: `{}`", _0.user_display())]
    PublishPrepare(PathBuf, #[source] Box<PublishPrepareError>),
    #[error("Failed to publish `{}` to {}", _0.user_display(), _1)]
    PublishSend(PathBuf, Url, #[source] PublishSendError),
}

/// Failure to get the metadata for a specific file.
#[derive(Error, Debug)]
pub enum PublishPrepareError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("Failed to read metadata")]
    Metadata(#[from] uv_metadata::Error),
    #[error("Failed to read metadata")]
    Metadata23(#[from] MetadataError),
    #[error("Only files ending in `.tar.gz` are valid source distributions: `{0}`")]
    InvalidExtension(SourceDistFilename),
    #[error("No PKG-INFO file found")]
    MissingPkgInfo,
    #[error("Multiple PKG-INFO files found: `{0}`")]
    MultiplePkgInfo(String),
    #[error("Failed to read: `{0}`")]
    Read(String, #[source] io::Error),
}

/// Failure in or after (HTTP) transport for a specific file.
#[derive(Error, Debug)]
pub enum PublishSendError {
    #[error("Failed to send POST request")]
    ReqwestMiddleware(#[from] reqwest_middleware::Error),
    #[error("Upload failed with status {0}")]
    StatusNoBody(StatusCode, #[source] reqwest::Error),
    #[error("Upload failed with status code {0}: {1}")]
    Status(StatusCode, String),
    /// The registry returned a "403 Forbidden".
    #[error("Permission denied (status code {0}): {1}")]
    PermissionDenied(StatusCode, String),
    /// See inline comment.
    #[error("The request was redirected, but redirects are not allowed when publishing, please use the canonical URL: `{0}`")]
    RedirectError(Url),
}

pub trait Reporter: Send + Sync + 'static {
    fn on_progress(&self, name: &str, id: usize);
    fn on_download_start(&self, name: &str, size: Option<u64>) -> usize;
    fn on_download_progress(&self, id: usize, inc: u64);
    fn on_download_complete(&self);
}

impl PublishSendError {
    /// Extract `code` from the PyPI json error response, if any.
    ///
    /// The error response from PyPI contains crucial context, such as the difference between
    /// "Invalid or non-existent authentication information" and "The user 'konstin' isn't allowed
    /// to upload to project 'dummy'".
    ///
    /// Twine uses the HTTP status reason for its error messages. In HTTP 2.0 and onward this field
    /// is abolished, so reqwest doesn't expose it, see
    /// <https://docs.rs/reqwest/0.12.7/reqwest/struct.StatusCode.html#method.canonical_reason>.
    /// PyPI does respect the content type for error responses and can return an error display as
    /// HTML, JSON and plain. Since HTML and plain text are both overly verbose, we show the JSON
    /// response. Examples are shown below, line breaks were inserted for readability. Of those,
    /// the `code` seems to be the most helpful message, so we return it. If the response isn't a
    /// JSON document with `code` we return the regular body.
    ///
    /// ```json
    /// {"message": "The server could not comply with the request since it is either malformed or
    /// otherwise incorrect.\n\n\nError: Use 'source' as Python version for an sdist.\n\n",
    /// "code": "400 Error: Use 'source' as Python version for an sdist.",
    /// "title": "Bad Request"}
    /// ```
    ///
    /// ```json
    /// {"message": "Access was denied to this resource.\n\n\nInvalid or non-existent authentication
    /// information. See https://test.pypi.org/help/#invalid-auth for more information.\n\n",
    /// "code": "403 Invalid or non-existent authentication information. See
    /// https://test.pypi.org/help/#invalid-auth for more information.",
    /// "title": "Forbidden"}
    /// ```
    /// ```json
    /// {"message": "Access was denied to this resource.\n\n\n\n\n",
    /// "code": "403 Username/Password authentication is no longer supported. Migrate to API
    /// Tokens or Trusted Publishers instead. See https://test.pypi.org/help/#apitoken and
    /// https://test.pypi.org/help/#trusted-publishers",
    /// "title": "Forbidden"}
    /// ```
    ///
    /// For context, for the last case twine shows:
    /// ```text
    /// WARNING  Error during upload. Retry with the --verbose option for more details.
    /// ERROR    HTTPError: 403 Forbidden from https://test.pypi.org/legacy/
    ///          Username/Password authentication is no longer supported. Migrate to API
    ///          Tokens or Trusted Publishers instead. See
    ///          https://test.pypi.org/help/#apitoken and
    ///          https://test.pypi.org/help/#trusted-publishers
    /// ```
    ///
    /// ```text
    /// INFO     Response from https://test.pypi.org/legacy/:
    ///          403 Username/Password authentication is no longer supported. Migrate to
    ///          API Tokens or Trusted Publishers instead. See
    ///          https://test.pypi.org/help/#apitoken and
    ///          https://test.pypi.org/help/#trusted-publishers
    /// INFO     <html>
    ///           <head>
    ///            <title>403 Username/Password authentication is no longer supported.
    ///          Migrate to API Tokens or Trusted Publishers instead. See
    ///          https://test.pypi.org/help/#apitoken and
    ///          https://test.pypi.org/help/#trusted-publishers</title>
    ///           </head>
    ///          <body>
    ///           <h1>403 Username/Password authentication is no longer supported.
    ///         Migrate to API Tokens or Trusted Publishers instead. See
    ///          https://test.pypi.org/help/#apitoken and
    ///          https://test.pypi.org/help/#trusted-publishers</h1>
    ///            Access was denied to this resource.<br/><br/>
    /// ```
    ///
    /// In comparison, we now show (line-wrapped for readability):
    ///
    /// ```text
    /// error: Failed to publish `dist/astral_test_1-0.1.0-py3-none-any.whl` to `https://test.pypi.org/legacy/`
    ///   Caused by: Incorrect credentials (status code 403 Forbidden): 403 Username/Password
    ///     authentication is no longer supported. Migrate to API Tokens or Trusted Publishers
    ///     instead. See https://test.pypi.org/help/#apitoken and https://test.pypi.org/help/#trusted-publishers
    /// ```
    fn extract_error_message(body: String, content_type: Option<&str>) -> String {
        if content_type == Some("application/json") {
            #[derive(Deserialize)]
            struct ErrorBody {
                code: String,
            }

            if let Ok(structured) = serde_json::from_str::<ErrorBody>(&body) {
                structured.code
            } else {
                body
            }
        } else {
            body
        }
    }
}

pub fn files_for_publishing(
    paths: Vec<String>,
) -> Result<Vec<(PathBuf, DistFilename)>, PublishError> {
    let mut seen = FxHashSet::default();
    let mut files = Vec::new();
    for path in paths {
        for dist in glob(&path).map_err(|err| PublishError::Pattern(path, err))? {
            let dist = dist?;
            if !dist.is_file() {
                continue;
            }
            if !seen.insert(dist.clone()) {
                continue;
            }
            let Some(filename) = dist.file_name().and_then(|filename| filename.to_str()) else {
                continue;
            };
            let filename = DistFilename::try_from_normalized_filename(filename)
                .ok_or_else(|| PublishError::InvalidFilename(dist.clone()))?;
            files.push((dist, filename));
        }
    }
    // TODO(konsti): Should we sort those files, e.g. wheels before sdists because they are more
    // certain to have reliable metadata, even though the metadata in the upload API is unreliable
    // in general?
    Ok(files)
}

/// Upload a file to a registry.
///
/// Returns `true` if the file was newly uploaded and `false` if it already existed.
pub async fn upload(
    file: &Path,
    filename: &DistFilename,
    registry: &Url,
    client: &BaseClient,
    username: Option<&str>,
    password: Option<&str>,
    reporter: Arc<impl Reporter>,
) -> Result<bool, PublishError> {
    let form_metadata = form_metadata(file, filename)
        .await
        .map_err(|err| PublishError::PublishPrepare(file.to_path_buf(), Box::new(err)))?;
    let request = build_request(
        file,
        filename,
        registry,
        client,
        username,
        password,
        form_metadata,
        reporter,
    )
    .await
    .map_err(|err| PublishError::PublishPrepare(file.to_path_buf(), Box::new(err)))?;

    let response = request.send().await.map_err(|err| {
        PublishError::PublishSend(file.to_path_buf(), registry.clone(), err.into())
    })?;

    handle_response(registry, response)
        .await
        .map_err(|err| PublishError::PublishSend(file.to_path_buf(), registry.clone(), err))
}

/// Calculate the SHA256 of a file.
fn hash_file(path: impl AsRef<Path>) -> Result<String, io::Error> {
    // Ideally, this would be async, but in case we actually want to make parallel uploads we should
    // use `spawn_blocking` since sha256 is cpu intensive.
    let mut file = BufReader::new(File::open(path.as_ref())?);
    let mut hasher = Sha256::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

// Not in `uv-metadata` because we only support tar files here.
async fn source_dist_pkg_info(file: &Path) -> Result<Vec<u8>, PublishPrepareError> {
    let file = fs_err::tokio::File::open(&file).await?;
    let reader = tokio::io::BufReader::new(file);
    let decoded = async_compression::tokio::bufread::GzipDecoder::new(reader);
    let mut archive = tokio_tar::Archive::new(decoded);
    let mut pkg_infos: Vec<(PathBuf, Vec<u8>)> = archive
        .entries()?
        .map_err(PublishPrepareError::from)
        .try_filter_map(|mut entry| async move {
            let path = entry
                .path()
                .map_err(PublishPrepareError::from)?
                .to_path_buf();
            let mut components = path.components();
            let Some(_top_level) = components.next() else {
                return Ok(None);
            };
            let Some(pkg_info) = components.next() else {
                return Ok(None);
            };
            if components.next().is_some() || pkg_info.as_os_str() != "PKG-INFO" {
                return Ok(None);
            }
            let mut buffer = Vec::new();
            // We have to read while iterating or the entry is empty as we're beyond it in the file.
            entry.read_to_end(&mut buffer).await.map_err(|err| {
                PublishPrepareError::Read(path.to_string_lossy().to_string(), err)
            })?;
            Ok(Some((path, buffer)))
        })
        .try_collect()
        .await?;
    match pkg_infos.len() {
        0 => Err(PublishPrepareError::MissingPkgInfo),
        1 => Ok(pkg_infos.remove(0).1),
        _ => Err(PublishPrepareError::MultiplePkgInfo(
            pkg_infos
                .iter()
                .map(|(path, _buffer)| path.to_string_lossy())
                .join(", "),
        )),
    }
}

async fn metadata(file: &Path, filename: &DistFilename) -> Result<Metadata23, PublishPrepareError> {
    let contents = match filename {
        DistFilename::SourceDistFilename(source_dist) => {
            if source_dist.extension != SourceDistExtension::TarGz {
                // See PEP 625. While we support installing legacy source distributions, we don't
                // support creating and uploading them.
                return Err(PublishPrepareError::InvalidExtension(source_dist.clone()));
            }
            source_dist_pkg_info(file).await?
        }
        DistFilename::WheelFilename(wheel) => {
            let file = fs_err::tokio::File::open(&file).await?;
            let reader = tokio::io::BufReader::new(file);
            read_metadata_async_seek(wheel, reader).await?
        }
    };
    Ok(Metadata23::parse(&contents)?)
}

/// Collect the non-file fields for the multipart request from the package METADATA.
///
/// Reference implementation: <https://github.com/pypi/warehouse/blob/d2c36d992cf9168e0518201d998b2707a3ef1e72/warehouse/forklift/legacy.py#L1376-L1430>
async fn form_metadata(
    file: &Path,
    filename: &DistFilename,
) -> Result<Vec<(&'static str, String)>, PublishPrepareError> {
    let hash_hex = hash_file(file)?;

    let metadata = metadata(file, filename).await?;

    let mut form_metadata = vec![
        (":action", "file_upload".to_string()),
        ("sha256_digest", hash_hex),
        ("protocol_version", "1".to_string()),
        ("metadata_version", metadata.metadata_version.clone()),
        // Twine transforms the name with `re.sub("[^A-Za-z0-9.]+", "-", name)`
        // * <https://github.com/pypa/twine/issues/743>
        // * <https://github.com/pypa/twine/blob/5bf3f38ff3d8b2de47b7baa7b652c697d7a64776/twine/package.py#L57-L65>
        // warehouse seems to call `packaging.utils.canonicalize_name` nowadays and has a separate
        // `normalized_name`, so we'll start with this and we'll readjust if there are user reports.
        ("name", metadata.name.clone()),
        ("version", metadata.version.clone()),
        ("filetype", filename.filetype().to_string()),
    ];

    if let DistFilename::WheelFilename(wheel) = filename {
        form_metadata.push(("pyversion", wheel.python_tag.join(".")));
    } else {
        form_metadata.push(("pyversion", "source".to_string()));
    }

    let mut add_option = |name, value: Option<String>| {
        if let Some(some) = value.clone() {
            form_metadata.push((name, some));
        }
    };

    add_option("summary", metadata.summary);
    add_option("description", metadata.description);
    add_option(
        "description_content_type",
        metadata.description_content_type,
    );
    add_option("author", metadata.author);
    add_option("author_email", metadata.author_email);
    add_option("maintainer", metadata.maintainer);
    add_option("maintainer_email", metadata.maintainer_email);
    add_option("license", metadata.license);
    add_option("keywords", metadata.keywords);
    add_option("home_page", metadata.home_page);
    add_option("download_url", metadata.download_url);

    // The GitLab PyPI repository API implementation requires this metadata field and twine always
    // includes it in the request, even when it's empty.
    form_metadata.push((
        "requires_python",
        metadata.requires_python.unwrap_or(String::new()),
    ));

    let mut add_vec = |name, values: Vec<String>| {
        for i in values {
            form_metadata.push((name, i.clone()));
        }
    };

    add_vec("classifiers", metadata.classifiers);
    add_vec("platform", metadata.platforms);
    add_vec("requires_dist", metadata.requires_dist);
    add_vec("provides_dist", metadata.provides_dist);
    add_vec("obsoletes_dist", metadata.obsoletes_dist);
    add_vec("requires_external", metadata.requires_external);
    add_vec("project_urls", metadata.project_urls);

    Ok(form_metadata)
}

async fn build_request(
    file: &Path,
    filename: &DistFilename,
    registry: &Url,
    client: &BaseClient,
    username: Option<&str>,
    password: Option<&str>,
    form_metadata: Vec<(&'static str, String)>,
    reporter: Arc<impl Reporter>,
) -> Result<RequestBuilder, PublishPrepareError> {
    let mut form = reqwest::multipart::Form::new();
    for (key, value) in form_metadata {
        form = form.text(key, value);
    }

    let file = fs_err::tokio::File::open(file).await?;
    let idx = reporter.on_download_start(&filename.to_string(), Some(file.metadata().await?.len()));
    let reader = ProgressReader::new(file, move |read| {
        reporter.on_download_progress(idx, read as u64);
    });
    // Stream wrapping puts a static lifetime requirement on the reader (so the request doesn't have
    // a lifetime) -> callback needs to be static -> reporter reference needs to be Arc'd.
    let file_reader = Body::wrap_stream(ReaderStream::new(reader));
    let part = Part::stream(file_reader).file_name(filename.to_string());
    form = form.part("content", part);

    let url = if let Some(username) = username {
        if password.is_none() {
            // Attach the username to the URL so the authentication middleware can find the matching
            // password.
            let mut url = registry.clone();
            let _ = url.set_username(username);
            url
        } else {
            // We set the authorization header below.
            registry.clone()
        }
    } else {
        registry.clone()
    };

    let mut request = client
        .client()
        .post(url)
        .multipart(form)
        // Ask PyPI for a structured error messages instead of HTML-markup error messages.
        // For other registries, we ask them to return plain text over HTML. See
        // [`PublishSendError::extract_remote_error`].
        .header(
            reqwest::header::ACCEPT,
            "application/json;q=0.9, text/plain;q=0.8, text/html;q=0.7",
        );
    if let (Some(username), Some(password)) = (username, password) {
        debug!("Using username/password basic auth");
        let credentials = BASE64_STANDARD.encode(format!("{username}:{password}"));
        request = request.header(AUTHORIZATION, format!("Basic {credentials}"));
    }
    Ok(request)
}

/// Returns `true` if the file was newly uploaded and `false` if it already existed.
async fn handle_response(registry: &Url, response: Response) -> Result<bool, PublishSendError> {
    let status_code = response.status();
    debug!("Response code for {registry}: {status_code}");
    trace!("Response headers for {registry}: {response:?}");

    // When the user accidentally uses https://test.pypi.org/simple (no slash) as publish URL, we
    // get a redirect to https://test.pypi.org/simple/ (the canonical index URL), while changing the
    // method to GET (see https://en.wikipedia.org/wiki/Post/Redirect/Get and
    // https://fetch.spec.whatwg.org/#http-redirect-fetch). The user gets a 200 OK while we actually
    // didn't upload anything! Reqwest doesn't support redirect policies conditional on the HTTP
    // method (https://github.com/seanmonstar/reqwest/issues/1777#issuecomment-2303386160), so we're
    // checking after the fact.
    if response.url() != registry {
        return Err(PublishSendError::RedirectError(response.url().clone()));
    }

    if status_code.is_success() {
        if enabled!(Level::TRACE) {
            match response.text().await {
                Ok(response_content) => {
                    trace!("Response content for {registry}: {response_content}");
                }
                Err(err) => {
                    trace!("Failed to read response content for {registry}: {err}");
                }
            }
        }
        return Ok(true);
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|content_type| content_type.to_str().ok())
        .map(ToString::to_string);
    let upload_error = response
        .bytes()
        .await
        .map_err(|err| PublishSendError::StatusNoBody(status_code, err))?;
    let upload_error = String::from_utf8_lossy(&upload_error);

    trace!("Response content for non-200 for {registry}: {upload_error}");

    debug!("Upload error response: {upload_error}");
    // Detect existing file errors the way twine does.
    // https://github.com/pypa/twine/blob/c512bbf166ac38239e58545a39155285f8747a7b/twine/commands/upload.py#L34-L72
    if status_code == 403 {
        if upload_error.contains("overwrite artifact") {
            // Artifactory (https://jfrog.com/artifactory/)
            Ok(false)
        } else {
            Err(PublishSendError::PermissionDenied(
                status_code,
                PublishSendError::extract_error_message(
                    upload_error.to_string(),
                    content_type.as_deref(),
                ),
            ))
        }
    } else if status_code == 409 {
        // conflict, pypiserver (https://pypi.org/project/pypiserver)
        Ok(false)
    } else if status_code == 400
        && (upload_error.contains("updating asset") || upload_error.contains("already been taken"))
    {
        // Nexus Repository OSS (https://www.sonatype.com/nexus-repository-oss)
        // and Gitlab Enterprise Edition (https://about.gitlab.com)
        Ok(false)
    } else {
        Err(PublishSendError::Status(
            status_code,
            PublishSendError::extract_error_message(
                upload_error.to_string(),
                content_type.as_deref(),
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use crate::{build_request, form_metadata, Reporter};
    use distribution_filename::DistFilename;
    use insta::{assert_debug_snapshot, assert_snapshot};
    use itertools::Itertools;
    use std::path::PathBuf;
    use std::sync::Arc;
    use url::Url;
    use uv_client::BaseClientBuilder;

    struct DummyReporter;

    impl Reporter for DummyReporter {
        fn on_progress(&self, _name: &str, _id: usize) {}
        fn on_download_start(&self, _name: &str, _size: Option<u64>) -> usize {
            0
        }
        fn on_download_progress(&self, _id: usize, _inc: u64) {}
        fn on_download_complete(&self) {}
    }

    /// Snapshot the data we send for an upload request for a source distribution.
    #[tokio::test]
    async fn upload_request_source_dist() {
        let filename = "tqdm-999.0.0.tar.gz";
        let file = PathBuf::from("../../scripts/links/").join(filename);
        let filename = DistFilename::try_from_normalized_filename(filename).unwrap();

        let form_metadata = form_metadata(&file, &filename).await.unwrap();

        let formatted_metadata = form_metadata
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .join("\n");
        assert_snapshot!(&formatted_metadata, @r###"
        :action: file_upload
        sha256_digest: 89fa05cffa7f457658373b85de302d24d0c205ceda2819a8739e324b75e9430b
        protocol_version: 1
        metadata_version: 2.3
        name: tqdm
        version: 999.0.0
        filetype: sdist
        pyversion: source
        description: # tqdm

        [![PyPI - Version](https://img.shields.io/pypi/v/tqdm.svg)](https://pypi.org/project/tqdm)
        [![PyPI - Python Version](https://img.shields.io/pypi/pyversions/tqdm.svg)](https://pypi.org/project/tqdm)

        -----

        **Table of Contents**

        - [Installation](#installation)
        - [License](#license)

        ## Installation

        ```console
        pip install tqdm
        ```

        ## License

        `tqdm` is distributed under the terms of the [MIT](https://spdx.org/licenses/MIT.html) license.

        description_content_type: text/markdown
        author_email: Charlie Marsh <charlie.r.marsh@gmail.com>
        requires_python: >=3.8
        classifiers: Development Status :: 4 - Beta
        classifiers: Programming Language :: Python
        classifiers: Programming Language :: Python :: 3.8
        classifiers: Programming Language :: Python :: 3.9
        classifiers: Programming Language :: Python :: 3.10
        classifiers: Programming Language :: Python :: 3.11
        classifiers: Programming Language :: Python :: 3.12
        classifiers: Programming Language :: Python :: Implementation :: CPython
        classifiers: Programming Language :: Python :: Implementation :: PyPy
        project_urls: Documentation, https://github.com/unknown/tqdm#readme
        project_urls: Issues, https://github.com/unknown/tqdm/issues
        project_urls: Source, https://github.com/unknown/tqdm
        "###);

        let request = build_request(
            &file,
            &filename,
            &Url::parse("https://example.org/upload").unwrap(),
            &BaseClientBuilder::new().build(),
            Some("ferris"),
            Some("F3RR!S"),
            form_metadata,
            Arc::new(DummyReporter),
        )
        .await
        .unwrap();

        insta::with_settings!({
            filters => [("boundary=[0-9a-f-]+", "boundary=[...]")],
        }, {
            assert_debug_snapshot!(&request, @r###"
            RequestBuilder {
                inner: RequestBuilder {
                    method: POST,
                    url: Url {
                        scheme: "https",
                        cannot_be_a_base: false,
                        username: "",
                        password: None,
                        host: Some(
                            Domain(
                                "example.org",
                            ),
                        ),
                        port: None,
                        path: "/upload",
                        query: None,
                        fragment: None,
                    },
                    headers: {
                        "content-type": "multipart/form-data; boundary=[...]",
                        "accept": "application/json;q=0.9, text/plain;q=0.8, text/html;q=0.7",
                        "authorization": "Basic ZmVycmlzOkYzUlIhUw==",
                    },
                },
                ..
            }
            "###);
        });
    }

    /// Snapshot the data we send for an upload request for a wheel.
    #[tokio::test]
    async fn upload_request_wheel() {
        let filename = "tqdm-4.66.1-py3-none-manylinux_2_12_x86_64.manylinux2010_x86_64.musllinux_1_1_x86_64.whl";
        let file = PathBuf::from("../../scripts/links/").join(filename);
        let filename = DistFilename::try_from_normalized_filename(filename).unwrap();

        let form_metadata = form_metadata(&file, &filename).await.unwrap();

        let formatted_metadata = form_metadata
            .iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .join("\n");
        assert_snapshot!(&formatted_metadata, @r###"
        :action: file_upload
        sha256_digest: 0d88ca657bc6b64995ca416e0c59c71af85cc10015d940fa446c42a8b485ee1c
        protocol_version: 1
        metadata_version: 2.1
        name: tqdm
        version: 4.66.1
        filetype: bdist_wheel
        pyversion: py3
        summary: Fast, Extensible Progress Meter
        description_content_type: text/x-rst
        maintainer_email: tqdm developers <devs@tqdm.ml>
        license: MPL-2.0 AND MIT
        keywords: progressbar,progressmeter,progress,bar,meter,rate,eta,console,terminal,time
        requires_python: >=3.7
        classifiers: Development Status :: 5 - Production/Stable
        classifiers: Environment :: Console
        classifiers: Environment :: MacOS X
        classifiers: Environment :: Other Environment
        classifiers: Environment :: Win32 (MS Windows)
        classifiers: Environment :: X11 Applications
        classifiers: Framework :: IPython
        classifiers: Framework :: Jupyter
        classifiers: Intended Audience :: Developers
        classifiers: Intended Audience :: Education
        classifiers: Intended Audience :: End Users/Desktop
        classifiers: Intended Audience :: Other Audience
        classifiers: Intended Audience :: System Administrators
        classifiers: License :: OSI Approved :: MIT License
        classifiers: License :: OSI Approved :: Mozilla Public License 2.0 (MPL 2.0)
        classifiers: Operating System :: MacOS
        classifiers: Operating System :: MacOS :: MacOS X
        classifiers: Operating System :: Microsoft
        classifiers: Operating System :: Microsoft :: MS-DOS
        classifiers: Operating System :: Microsoft :: Windows
        classifiers: Operating System :: POSIX
        classifiers: Operating System :: POSIX :: BSD
        classifiers: Operating System :: POSIX :: BSD :: FreeBSD
        classifiers: Operating System :: POSIX :: Linux
        classifiers: Operating System :: POSIX :: SunOS/Solaris
        classifiers: Operating System :: Unix
        classifiers: Programming Language :: Python
        classifiers: Programming Language :: Python :: 3
        classifiers: Programming Language :: Python :: 3.7
        classifiers: Programming Language :: Python :: 3.8
        classifiers: Programming Language :: Python :: 3.9
        classifiers: Programming Language :: Python :: 3.10
        classifiers: Programming Language :: Python :: 3.11
        classifiers: Programming Language :: Python :: 3 :: Only
        classifiers: Programming Language :: Python :: Implementation
        classifiers: Programming Language :: Python :: Implementation :: IronPython
        classifiers: Programming Language :: Python :: Implementation :: PyPy
        classifiers: Programming Language :: Unix Shell
        classifiers: Topic :: Desktop Environment
        classifiers: Topic :: Education :: Computer Aided Instruction (CAI)
        classifiers: Topic :: Education :: Testing
        classifiers: Topic :: Office/Business
        classifiers: Topic :: Other/Nonlisted Topic
        classifiers: Topic :: Software Development :: Build Tools
        classifiers: Topic :: Software Development :: Libraries
        classifiers: Topic :: Software Development :: Libraries :: Python Modules
        classifiers: Topic :: Software Development :: Pre-processors
        classifiers: Topic :: Software Development :: User Interfaces
        classifiers: Topic :: System :: Installation/Setup
        classifiers: Topic :: System :: Logging
        classifiers: Topic :: System :: Monitoring
        classifiers: Topic :: System :: Shells
        classifiers: Topic :: Terminals
        classifiers: Topic :: Utilities
        requires_dist: colorama ; platform_system == "Windows"
        requires_dist: pytest >=6 ; extra == 'dev'
        requires_dist: pytest-cov ; extra == 'dev'
        requires_dist: pytest-timeout ; extra == 'dev'
        requires_dist: pytest-xdist ; extra == 'dev'
        requires_dist: ipywidgets >=6 ; extra == 'notebook'
        requires_dist: slack-sdk ; extra == 'slack'
        requires_dist: requests ; extra == 'telegram'
        project_urls: homepage, https://tqdm.github.io
        project_urls: repository, https://github.com/tqdm/tqdm
        project_urls: changelog, https://tqdm.github.io/releases
        project_urls: wiki, https://github.com/tqdm/tqdm/wiki
        "###);

        let request = build_request(
            &file,
            &filename,
            &Url::parse("https://example.org/upload").unwrap(),
            &BaseClientBuilder::new().build(),
            Some("ferris"),
            Some("F3RR!S"),
            form_metadata,
            Arc::new(DummyReporter),
        )
        .await
        .unwrap();

        insta::with_settings!({
            filters => [("boundary=[0-9a-f-]+", "boundary=[...]")],
        }, {
            assert_debug_snapshot!(&request, @r###"
            RequestBuilder {
                inner: RequestBuilder {
                    method: POST,
                    url: Url {
                        scheme: "https",
                        cannot_be_a_base: false,
                        username: "",
                        password: None,
                        host: Some(
                            Domain(
                                "example.org",
                            ),
                        ),
                        port: None,
                        path: "/upload",
                        query: None,
                        fragment: None,
                    },
                    headers: {
                        "content-type": "multipart/form-data; boundary=[...]",
                        "accept": "application/json;q=0.9, text/plain;q=0.8, text/html;q=0.7",
                        "authorization": "Basic ZmVycmlzOkYzUlIhUw==",
                    },
                },
                ..
            }
            "###);
        });
    }
}