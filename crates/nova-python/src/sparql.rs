use crate::io::*;
use crate::model::*;
use oxrdf::{Term, Triple, Variable};
use pyo3::IntoPyObjectExt;
use pyo3::exceptions::{PySyntaxError, PyValueError};
use pyo3::prelude::*;
use pyo3::pybacked::PyBackedStr;
use sparesults::{
    QueryResultsFormat, QueryResultsParseError, QueryResultsParser, QueryResultsSerializer,
    QuerySolution, ReaderQueryResultsParserOutput, ReaderSolutionsParser,
};
use spargebra::SparqlSyntaxError;
use oxigraph_nova_store::QueryResults;
use oxrdfio::RdfSerializer;
use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::vec::IntoIter;


/// Converts a Nova [`QueryResults`] into the Python object matching its query form:
/// an iterator of [`PyQuerySolution`] for `SELECT`, a [`PyQueryBoolean`] for `ASK`,
/// or an iterator of [`PyTriple`] for `CONSTRUCT`/`DESCRIBE`. `variables` gives the
/// ordered SELECT projection list needed to build each row's `sparesults::QuerySolution`
/// (Nova's own `Solution` only supports lookup by `&Variable`, not iteration/`.values()`).
pub fn query_results_to_python<'py>(
    py: Python<'py>,
    results: QueryResults,
    variables: Vec<Variable>,
) -> PyResult<Bound<'py, PyAny>> {
    match results {
        QueryResults::Solutions(solutions) => {
            let rows = solutions
                .into_iter()
                .map(|solution| {
                    let values: Vec<Option<Term>> = variables
                        .iter()
                        .map(|v| solution.get(v).cloned())
                        .collect();
                    QuerySolution::from((variables.clone(), values))
                })
                .collect::<Vec<_>>()
                .into_iter();
            PyQuerySolutions {
                inner: PyQuerySolutionsVariant::Query {
                    iter: rows,
                    variables,
                },
            }
            .into_bound_py_any(py)
        }
        QueryResults::Graph(triples) => PyQueryTriples {
            inner: triples.into_iter(),
        }
        .into_bound_py_any(py),
        QueryResults::Boolean(b) => PyQueryBoolean { inner: b }.into_bound_py_any(py),
    }
}

/// A solution of a SPARQL ``SELECT`` query.
///
/// It is the equivalent of a row in SQL.
///
/// It could be indexes by variable name (:py:class:`Variable` or :py:class:`str`) or position in the tuple (:py:class:`int`).
/// Unpacking also works.
///
/// >>> store = Store()
/// >>> store.add(Quad(NamedNode('http://example.com'), NamedNode('http://example.com/p'), Literal('1')))
/// >>> solution = next(store.query('SELECT ?s ?p ?o WHERE { ?s ?p ?o }'))
/// >>> solution[Variable('s')]
/// <NamedNode value=http://example.com>
/// >>> solution['s']
/// <NamedNode value=http://example.com>
/// >>> solution[0]
/// <NamedNode value=http://example.com>
/// >>> s, p, o = solution
/// >>> s
/// <NamedNode value=http://example.com>
#[pyclass(frozen, name = "QuerySolution", module = "pyoxigraph", eq)]
#[derive(Eq, PartialEq)]
pub struct PyQuerySolution {
    inner: QuerySolution,
}

#[pymethods]
impl PyQuerySolution {
    fn __repr__(&self) -> String {
        let mut buffer = String::new();
        buffer.push_str("<QuerySolution");
        for (k, v) in self.inner.iter() {
            buffer.push(' ');
            buffer.push_str(k.as_str());
            buffer.push('=');
            term_repr(v.as_ref(), &mut buffer)
        }
        buffer.push('>');
        buffer
    }

    fn __len__(&self) -> usize {
        self.inner.len()
    }

    fn __getitem__(&self, key: PySolutionKey<'_>) -> Option<PyTerm> {
        match key {
            PySolutionKey::Usize(key) => self.inner.get(key),
            PySolutionKey::Str(key) => {
                let k: &str = &key;
                self.inner.get(k)
            }
            PySolutionKey::Variable(key) => self.inner.get(<&Variable>::from(&*key)),
        }
        .map(|term| PyTerm::from(term.clone()))
    }

    fn __iter__(&self) -> SolutionValueIter {
        SolutionValueIter {
            inner: self.inner.values().to_vec().into_iter(),
        }
    }

    /// :rtype: QuerySolution
    fn __copy__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// :type memo: typing.Any
    /// :rtype: QuerySolution
    #[expect(unused_variables)]
    fn __deepcopy__<'a>(slf: PyRef<'a, Self>, memo: &'_ Bound<'_, PyAny>) -> PyRef<'a, Self> {
        slf
    }
}

#[derive(FromPyObject)]
pub enum PySolutionKey<'a> {
    Usize(usize),
    Str(PyBackedStr),
    Variable(PyRef<'a, PyVariable>),
}

#[pyclass(module = "pyoxigraph")]
pub struct SolutionValueIter {
    inner: IntoIter<Option<Term>>,
}

#[pymethods]
impl SolutionValueIter {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self) -> Option<Option<PyTerm>> {
        self.inner.next().map(|v| v.map(PyTerm::from))
    }
}

/// An iterator of :py:class:`QuerySolution` returned by a SPARQL ``SELECT`` query
///
/// >>> store = Store()
/// >>> store.add(Quad(NamedNode('http://example.com'), NamedNode('http://example.com/p'), Literal('1')))
/// >>> list(store.query('SELECT ?s WHERE { ?s ?p ?o }'))
/// [<QuerySolution s=<NamedNode value=http://example.com>>]
#[pyclass(unsendable, name = "QuerySolutions", module = "pyoxigraph")]
pub struct PyQuerySolutions {
    inner: PyQuerySolutionsVariant,
}

/// Nova's results are eagerly materialized into a `Vec` (unlike upstream's lazy,
/// possibly-fallible `QuerySolutionIter`), so the `Query` variant here is a plain,
/// infallible `Vec` iterator and needs no `unsafe impl Send` wrapper.
enum PyQuerySolutionsVariant {
    Query {
        iter: IntoIter<QuerySolution>,
        variables: Vec<Variable>,
    },
    Reader {
        iter: ReaderSolutionsParser<PyReadable>,
        file_path: Option<PathBuf>,
    },
}

#[pymethods]
impl PyQuerySolutions {
    /// :return: the ordered list of all variables that could appear in the query results
    /// :rtype: list[Variable]
    ///
    /// >>> store = Store()
    /// >>> store.query('SELECT ?s WHERE { ?s ?p ?o }').variables
    /// [<Variable value=s>]
    #[getter]
    fn variables(&self) -> Vec<PyVariable> {
        match &self.inner {
            PyQuerySolutionsVariant::Query { variables, .. } => {
                variables.iter().map(|v| v.clone().into()).collect()
            }
            PyQuerySolutionsVariant::Reader { iter, .. } => {
                iter.variables().iter().map(|v| v.clone().into()).collect()
            }
        }
    }

    /// Writes the query results into a file.
    ///
    /// It currently supports the following formats:
    ///
    /// * `XML <https://www.w3.org/TR/rdf-sparql-XMLres/>`_ (:py:attr:`QueryResultsFormat.XML`)
    /// * `JSON <https://www.w3.org/TR/sparql11-results-json/>`_ (:py:attr:`QueryResultsFormat.JSON`)
    /// * `CSV <https://www.w3.org/TR/sparql11-results-csv-tsv/>`_ (:py:attr:`QueryResultsFormat.CSV`)
    /// * `TSV <https://www.w3.org/TR/sparql11-results-csv-tsv/>`_ (:py:attr:`QueryResultsFormat.TSV`)
    ///
    /// :param output: The binary I/O object or file path to write to. For example, it could be a file path as a string or a file writer opened in binary mode with ``open('my_file.ttl', 'wb')``. If :py:const:`None`, a :py:class:`bytes` buffer is returned with the serialized content.
    /// :type output: typing.IO[bytes] or str or os.PathLike[str] or None, optional
    /// :param format: the format of the query results serialization. If :py:const:`None`, the format is guessed from the file name extension.
    /// :type format: QueryResultsFormat or None, optional
    /// :rtype: bytes or None
    /// :raises ValueError: if the format is not supported.
    /// :raises OSError: if a system error happens while writing the file.
    ///
    /// >>> store = Store()
    /// >>> store.add(Quad(NamedNode('http://example.com'), NamedNode('http://example.com/p'), Literal('1')))
    /// >>> results = store.query("SELECT ?s ?p ?o WHERE { ?s ?p ?o }")
    /// >>> results.serialize(format=QueryResultsFormat.JSON)
    /// b'{"head":{"vars":["s","p","o"]},"results":{"bindings":[{"s":{"type":"uri","value":"http://example.com"},"p":{"type":"uri","value":"http://example.com/p"},"o":{"type":"literal","value":"1"}}]}}'
    #[expect(clippy::doc_link_with_quotes)]
    #[pyo3(signature = (output = None, format = None))]
    fn serialize(
        &mut self,
        output: Option<PyWritableOutput>,
        format: Option<PyQueryResultsFormat>,
        py: Python<'_>,
    ) -> PyResult<Option<Vec<u8>>> {
        PyWritable::do_write(
            |output, file_path| {
                let format = lookup_query_results_format(format, file_path.as_deref())?;
                py.detach(|| {
                    let mut serializer = QueryResultsSerializer::from_format(format)
                        .serialize_solutions_to_writer(
                            output,
                            match &self.inner {
                                PyQuerySolutionsVariant::Query { variables, .. } => {
                                    variables.clone()
                                }
                                PyQuerySolutionsVariant::Reader { iter, .. } => {
                                    iter.variables().to_vec()
                                }
                            },
                        )?;
                    match &mut self.inner {
                        PyQuerySolutionsVariant::Query { iter, .. } => {
                            for solution in iter {
                                serializer.serialize(&solution)?;
                            }
                        }
                        PyQuerySolutionsVariant::Reader { iter, file_path } => {
                            for solution in iter {
                                serializer.serialize(&solution.map_err(|e| {
                                    map_query_results_parse_error(e, file_path.clone())
                                })?)?;
                            }
                        }
                    }

                    Ok(serializer.finish()?)
                })
            },
            output,
            py,
        )
    }

    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self, py: Python<'_>) -> PyResult<Option<PyQuerySolution>> {
        Ok(match &mut self.inner {
            PyQuerySolutionsVariant::Query { iter, .. } => Ok(iter.next()),
            PyQuerySolutionsVariant::Reader { iter, file_path } => py
                .detach(|| iter.next())
                .transpose()
                .map_err(|e| map_query_results_parse_error(e, file_path.clone())),
        }?
        .map(move |inner| PyQuerySolution { inner }))
    }
}

/// A boolean returned by a SPARQL ``ASK`` query.
///
/// It can be easily casted to a regular boolean using the :py:func:`bool` function.
///
/// >>> store = Store()
/// >>> store.add(Quad(NamedNode('http://example.com'), NamedNode('http://example.com/p'), Literal('1')))
/// >>> bool(store.query('ASK { ?s ?p ?o }'))
/// True
#[pyclass(frozen, name = "QueryBoolean", module = "pyoxigraph", eq, ord, hash)]
#[derive(Eq, Ord, PartialOrd, PartialEq, Hash)]
pub struct PyQueryBoolean {
    inner: bool,
}

#[pymethods]
impl PyQueryBoolean {
    /// Writes the query results into a file.
    ///
    /// It currently supports the following formats:
    ///
    /// * `XML <https://www.w3.org/TR/rdf-sparql-XMLres/>`_ (:py:attr:`QueryResultsFormat.XML`)
    /// * `JSON <https://www.w3.org/TR/sparql11-results-json/>`_ (:py:attr:`QueryResultsFormat.JSON`)
    /// * `CSV <https://www.w3.org/TR/sparql11-results-csv-tsv/>`_ (:py:attr:`QueryResultsFormat.CSV`)
    /// * `TSV <https://www.w3.org/TR/sparql11-results-csv-tsv/>`_ (:py:attr:`QueryResultsFormat.TSV`)
    ///
    /// :param output: The binary I/O object or file path to write to. For example, it could be a file path as a string or a file writer opened in binary mode with ``open('my_file.ttl', 'wb')``. If :py:const:`None`, a :py:class:`bytes` buffer is returned with the serialized content.
    /// :type output: typing.IO[bytes] or str or os.PathLike[str] or None, optional
    /// :param format: the format of the query results serialization. If :py:const:`None`, the format is guessed from the file name extension.
    /// :type format: QueryResultsFormat or None, optional
    /// :rtype: bytes or None
    /// :raises ValueError: if the format is not supported.
    /// :raises OSError: if a system error happens while writing the file.
    ///
    /// >>> store = Store()
    /// >>> store.add(Quad(NamedNode('http://example.com'), NamedNode('http://example.com/p'), Literal('1')))
    /// >>> results = store.query("ASK { ?s ?p ?o }")
    /// >>> results.serialize(format=QueryResultsFormat.JSON)
    /// b'{"head":{},"boolean":true}'
    #[pyo3(signature = (output = None, format = None))]
    fn serialize(
        &self,
        output: Option<PyWritableOutput>,
        format: Option<PyQueryResultsFormat>,
        py: Python<'_>,
    ) -> PyResult<Option<Vec<u8>>> {
        PyWritable::do_write(
            |output, file_path| {
                let format = lookup_query_results_format(format, file_path.as_deref())?;
                py.detach(|| {
                    Ok(QueryResultsSerializer::from_format(format)
                        .serialize_boolean_to_writer(output, self.inner)?)
                })
            },
            output,
            py,
        )
    }

    fn __bool__(&self) -> bool {
        self.inner
    }

    fn __repr__(&self) -> String {
        format!("<QueryBoolean {}>", self.inner)
    }
}

/// An iterator of :py:class:`Triple` returned by a SPARQL ``CONSTRUCT`` or ``DESCRIBE`` query
///
/// >>> store = Store()
/// >>> store.add(Quad(NamedNode('http://example.com'), NamedNode('http://example.com/p'), Literal('1')))
/// >>> list(store.query('CONSTRUCT WHERE { ?s ?p ?o }'))
/// [<Triple subject=<NamedNode value=http://example.com> predicate=<NamedNode value=http://example.com/p> object=<Literal value=1 datatype=<NamedNode value=http://www.w3.org/2001/XMLSchema#string>>>]
#[pyclass(unsendable, name = "QueryTriples", module = "pyoxigraph")]
pub struct PyQueryTriples {
    inner: IntoIter<Triple>,
}

#[pymethods]
impl PyQueryTriples {
    /// Writes the query results into a file.
    ///
    /// It currently supports the following formats:
    ///
    /// * `JSON-LD <https://www.w3.org/TR/json-ld/>`_ (:py:attr:`RdfFormat.JSON_LD`)
    /// * `canonical <https://www.w3.org/TR/n-triples/#canonical-ntriples>`_ `N-Triples <https://www.w3.org/TR/n-triples/>`_ (:py:attr:`RdfFormat.N_TRIPLES`)
    /// * `N-Quads <https://www.w3.org/TR/n-quads/>`_ (:py:attr:`RdfFormat.N_QUADS`)
    /// * `Turtle <https://www.w3.org/TR/turtle/>`_ (:py:attr:`RdfFormat.TURTLE`)
    /// * `TriG <https://www.w3.org/TR/trig/>`_ (:py:attr:`RdfFormat.TRIG`)
    /// * `N3 <https://w3c.github.io/N3/spec/>`_ (:py:attr:`RdfFormat.N3`)
    /// * `RDF/XML <https://www.w3.org/TR/rdf-syntax-grammar/>`_ (:py:attr:`RdfFormat.RDF_XML`)
    ///
    /// :param output: The binary I/O object or file path to write to. For example, it could be a file path as a string or a file writer opened in binary mode with ``open('my_file.ttl', 'wb')``. If :py:const:`None`, a :py:class:`bytes` buffer is returned with the serialized content.
    /// :type output: typing.IO[bytes] or str or os.PathLike[str] or None, optional
    /// :param format: the format of the RDF serialization. If :py:const:`None`, the format is guessed from the file name extension.
    /// :type format: RdfFormat or None, optional
    /// :rtype: bytes or None
    /// :raises ValueError: if the format is not supported.
    /// :raises OSError: if a system error happens while writing the file.
    ///
    /// >>> store = Store()
    /// >>> store.add(Quad(NamedNode('http://example.com'), NamedNode('http://example.com/p'), Literal('1')))
    /// >>> results = store.query("CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }")
    /// >>> results.serialize(format=RdfFormat.N_TRIPLES)
    /// b'<http://example.com> <http://example.com/p> "1" .\n'
    #[pyo3(signature = (output = None, format = None))]
    fn serialize(
        &mut self,
        output: Option<PyWritableOutput>,
        format: Option<PyRdfFormat>,
        py: Python<'_>,
    ) -> PyResult<Option<Vec<u8>>> {
        PyWritable::do_write(
            |output, file_path| {
                let format = lookup_rdf_format(format, file_path.as_deref())?;
                py.detach(move || {
                    let mut serializer = RdfSerializer::from_format(format).for_writer(output);
                    for triple in &mut self.inner {
                        serializer.serialize_triple(&triple)?;
                    }
                    Ok(serializer.finish()?)
                })
            },
            output,
            py,
        )
    }

    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self) -> Option<PyTriple> {
        self.inner.next().map(Into::into)
    }
}

/// Parses SPARQL query results.
///
/// It currently supports the following formats:
///
/// * `XML <https://www.w3.org/TR/rdf-sparql-XMLres/>`_ (:py:attr:`QueryResultsFormat.XML`)
/// * `JSON <https://www.w3.org/TR/sparql11-results-json/>`_ (:py:attr:`QueryResultsFormat.JSON`)
/// * `TSV <https://www.w3.org/TR/sparql11-results-csv-tsv/>`_ (:py:attr:`QueryResultsFormat.TSV`)
///
/// :param input: The :py:class:`str`, :py:class:`bytes` or I/O object to read from. For example, it could be the file content as a string or a file reader opened in binary mode with ``open('my_file.ttl', 'rb')``.
/// :type input: bytes or str or typing.IO[bytes] or typing.IO[str] or None, optional
/// :param format: the format of the query results serialization. If :py:const:`None`, the format is guessed from the file name extension.
/// :type format: QueryResultsFormat or None, optional
/// :param path: The file path to read from. Replaces the ``input`` parameter.
/// :type path: str or os.PathLike[str] or None, optional
/// :return: an iterator of :py:class:`QuerySolution` or a :py:class:`bool`.
/// :rtype: QuerySolutions or QueryBoolean
/// :raises ValueError: if the format is not supported.
/// :raises SyntaxError: if the provided data is invalid.
/// :raises OSError: if a system error happens while reading the file.
///
/// >>> list(parse_query_results('?s\t?p\t?o\n<http://example.com/s>\t<http://example.com/s>\t1\n', QueryResultsFormat.TSV))
/// [<QuerySolution s=<NamedNode value=http://example.com/s> p=<NamedNode value=http://example.com/s> o=<Literal value=1 datatype=<NamedNode value=http://www.w3.org/2001/XMLSchema#integer>>>]
///
/// >>> parse_query_results('{"head":{},"boolean":true}', QueryResultsFormat.JSON)
/// <QueryBoolean true>
#[pyfunction]
#[pyo3(signature = (input = None, format = None, *, path = None))]
pub fn parse_query_results(
    input: Option<PyReadableInput>,
    format: Option<PyQueryResultsFormat>,
    path: Option<PathBuf>,
    py: Python<'_>,
) -> PyResult<Bound<'_, PyAny>> {
    let input = PyReadable::from_args(&path, input, py)?;
    let format = lookup_query_results_format(format, path.as_deref())?;
    let results = QueryResultsParser::from_format(format)
        .for_reader(input)
        .map_err(|e| map_query_results_parse_error(e, path.clone()))?;
    match results {
        ReaderQueryResultsParserOutput::Solutions(iter) => PyQuerySolutions {
            inner: PyQuerySolutionsVariant::Reader {
                iter,
                file_path: path,
            },
        }
        .into_bound_py_any(py),
        ReaderQueryResultsParserOutput::Boolean(inner) => {
            PyQueryBoolean { inner }.into_bound_py_any(py)
        }
    }
}

/// `SPARQL query <https://www.w3.org/TR/sparql11-query/>`_ results serialization formats.
///
/// The following formats are supported:
///
/// * `XML <https://www.w3.org/TR/rdf-sparql-XMLres/>`_ (:py:attr:`QueryResultsFormat.XML`)
/// * `JSON <https://www.w3.org/TR/sparql11-results-json/>`_ (:py:attr:`QueryResultsFormat.JSON`)
/// * `CSV <https://www.w3.org/TR/sparql11-results-csv-tsv/>`_ (:py:attr:`QueryResultsFormat.CSV`)
/// * `TSV <https://www.w3.org/TR/sparql11-results-csv-tsv/>`_ (:py:attr:`QueryResultsFormat.TSV`)
#[pyclass(
    frozen,
    name = "QueryResultsFormat",
    module = "pyoxigraph",
    eq,
    hash,
    str,
    from_py_object
)]
#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct PyQueryResultsFormat {
    inner: QueryResultsFormat,
}

#[pymethods]
impl PyQueryResultsFormat {
    /// `SPARQL Query Results CSV Format <https://www.w3.org/TR/sparql11-results-csv-tsv/>`_
    #[classattr]
    const CSV: Self = Self {
        inner: QueryResultsFormat::Csv,
    };
    /// `SPARQL Query Results JSON Format <https://www.w3.org/TR/sparql11-results-json/>`_
    #[classattr]
    const JSON: Self = Self {
        inner: QueryResultsFormat::Json,
    };
    /// `SPARQL Query Results TSV Format <https://www.w3.org/TR/sparql11-results-csv-tsv/>`_
    #[classattr]
    const TSV: Self = Self {
        inner: QueryResultsFormat::Tsv,
    };
    /// `SPARQL Query Results XML Format <https://www.w3.org/TR/rdf-sparql-XMLres/>`_
    #[classattr]
    const XML: Self = Self {
        inner: QueryResultsFormat::Xml,
    };

    /// :return: the format canonical IRI according to the `Unique URIs for file formats registry <https://www.w3.org/ns/formats/>`_.
    /// :rtype: str
    ///
    /// >>> QueryResultsFormat.JSON.iri
    /// 'http://www.w3.org/ns/formats/SPARQL_Results_JSON'
    #[getter]
    fn iri(&self) -> &'static str {
        self.inner.iri()
    }

    /// :return: the format `IANA media type <https://tools.ietf.org/html/rfc2046>`_.
    /// :rtype: str
    ///
    /// >>> QueryResultsFormat.JSON.media_type
    /// 'application/sparql-results+json'
    #[getter]
    fn media_type(&self) -> &'static str {
        self.inner.media_type()
    }

    /// :return: the format `IANA-registered <https://tools.ietf.org/html/rfc2046>`_ file extension.
    /// :rtype: str
    ///
    /// >>> QueryResultsFormat.JSON.file_extension
    /// 'srj'
    #[getter]
    fn file_extension(&self) -> &'static str {
        self.inner.file_extension()
    }

    /// :return: the format name.
    /// :rtype: str
    ///
    /// >>> QueryResultsFormat.JSON.name
    /// 'SPARQL Results in JSON'
    #[getter]
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    /// Looks for a known format from a media type.
    ///
    /// It supports some media type aliases.
    /// For example, "application/xml" is going to return :py:const:`QueryResultsFormat.XML` even if it is not its canonical media type.
    ///
    /// :param media_type: the media type.
    /// :type media_type: str
    /// :return: :py:class:`QueryResultsFormat` if the media type is known or :py:const:`None` if not.
    /// :rtype: QueryResultsFormat or None
    ///
    /// >>> QueryResultsFormat.from_media_type("application/sparql-results+json; charset=utf-8")
    /// <QueryResultsFormat SPARQL Results in JSON>
    #[staticmethod]
    fn from_media_type(media_type: &str) -> Option<Self> {
        Some(Self {
            inner: QueryResultsFormat::from_media_type(media_type)?,
        })
    }

    /// Looks for a known format from an extension.
    ///
    /// It supports some aliases.
    ///
    /// :param extension: the extension.
    /// :type extension: str
    /// :return: :py:class:`QueryResultsFormat` if the extension is known or :py:const:`None` if not.
    /// :rtype: QueryResultsFormat or None
    ///
    /// >>> QueryResultsFormat.from_extension("json")
    /// <QueryResultsFormat SPARQL Results in JSON>
    #[staticmethod]
    fn from_extension(extension: &str) -> Option<Self> {
        Some(Self {
            inner: QueryResultsFormat::from_extension(extension)?,
        })
    }

    fn __repr__(&self) -> String {
        format!("<QueryResultsFormat {}>", self.inner.name())
    }

    /// :rtype: QueryResultsFormat
    fn __copy__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// :type memo: typing.Any
    /// :rtype: QueryResultsFormat
    #[expect(unused_variables)]
    fn __deepcopy__<'a>(slf: PyRef<'a, Self>, memo: &'_ Bound<'_, PyAny>) -> PyRef<'a, Self> {
        slf
    }
}

impl fmt::Display for PyQueryResultsFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}

fn lookup_query_results_format(
    format: Option<PyQueryResultsFormat>,
    path: Option<&Path>,
) -> PyResult<QueryResultsFormat> {
    if let Some(format) = format {
        return Ok(format.inner);
    }
    let Some(path) = path else {
        return Err(PyValueError::new_err(
            "The format parameter is required when a file path is not given",
        ));
    };
    let Some(ext) = path.extension().and_then(OsStr::to_str) else {
        return Err(PyValueError::new_err(format!(
            "The file name {} has no extension to guess a file format from",
            path.display()
        )));
    };
    QueryResultsFormat::from_extension(ext)
        .ok_or_else(|| PyValueError::new_err(format!("Not supported RDF format extension: {ext}")))
}

/// Maps a `spargebra` SPARQL parse error to a Python `SyntaxError`. Unlike upstream's
/// `oxigraph::sparql::SparqlSyntaxError`, Nova's `spargebra::SparqlSyntaxError` does not
/// expose a `.location()` method, so this can only report the error message, not the
/// exact line/column range.
pub(crate) fn map_sparql_syntax_error(error: SparqlSyntaxError) -> PyErr {
    PySyntaxError::new_err(error.to_string())
}

pub fn map_query_results_parse_error(
    error: QueryResultsParseError,
    file_path: Option<PathBuf>,
) -> PyErr {
    match error {
        QueryResultsParseError::Syntax(error) => {
            // Python 3.9 does not support end line and end column
            if python_version() >= (3, 10) {
                let params = if let Some(location) = error.location() {
                    (
                        file_path.map(PathBuf::into_os_string),
                        Some(location.start.line + 1),
                        Some(location.start.column + 1),
                        None::<Vec<u8>>,
                        Some(location.end.line + 1),
                        Some(location.end.column + 1),
                    )
                } else {
                    (None, None, None, None, None, None)
                };
                PySyntaxError::new_err((error.to_string(), params))
            } else {
                let params = if let Some(location) = error.location() {
                    (
                        file_path.map(PathBuf::into_os_string),
                        Some(location.start.line + 1),
                        Some(location.start.column + 1),
                        None::<Vec<u8>>,
                    )
                } else {
                    (None, None, None, None)
                };
                PySyntaxError::new_err((error.to_string(), params))
            }
        }
        QueryResultsParseError::Io(error) => error.into(),
    }
}
