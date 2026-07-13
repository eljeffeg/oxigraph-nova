use crate::io::{
    PyIo, PyRdfFormat, PyReadableInput, PyWritable, PyWritableOutput, lookup_rdf_format,
};
use crate::model::*;
use crate::sparql::*;
use oxigraph_nova_core::{GraphName, NamedOrBlankNode, Quad, Term};
use oxigraph_nova_query::QueryOptions;
use oxigraph_nova_store::Store;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use spargebra::SparqlParser;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs::File;
use std::path::PathBuf;

/// RDF store.
///
/// It encodes a `RDF dataset <https://www.w3.org/TR/rdf11-concepts/#dfn-rdf-dataset>`_ and allows to query it using SPARQL.
/// It is backed by Oxigraph Nova's Ring storage engine.
///
/// This store ensures the "repeatable read" isolation level: the store only exposes changes that have
/// been "committed" (i.e. no partial writes) and the exposed state does not change for the complete duration
/// of a read operation (e.g. a SPARQL query) or a read/write operation (e.g. a SPARQL update).
///
/// :param path: the path of the directory in which the store should read and write its data. If the directory does not exist, it is created.
///              If no directory is provided a temporary one is created and removed when the Python garbage collector removes the store.
///              In this case, the store data are kept in memory and never written on disk.
/// :type path: str or os.PathLike[str] or None, optional
/// :raises OSError: if the target directory contains invalid data or could not be accessed.
///
/// The :py:class:`str` function provides a serialization of the store in NQuads:
///
/// >>> store = Store()
/// >>> store.add(Quad(NamedNode('http://example.com'), NamedNode('http://example.com/p'), Literal('1'), NamedNode('http://example.com/g')))
/// >>> str(store)
/// '<http://example.com> <http://example.com/p> "1" <http://example.com/g> .\n'
#[pyclass(frozen, name = "Store", module = "pyoxigraph", str)]
pub struct PyStore {
    inner: Store,
}

#[pymethods]
impl PyStore {
    #[new]
    #[pyo3(signature = (path = None))]
    fn new(path: Option<PathBuf>, py: Python<'_>) -> PyResult<Self> {
        py.detach(|| {
            Ok(Self {
                inner: if let Some(path) = path {
                    Store::open(path)
                } else {
                    Ok(Store::new())
                }
                .map_err(map_anyhow_error)?,
            })
        })
    }

    /// Adds a quad to the store.
    ///
    /// :param quad: the quad to add.
    /// :type quad: Quad
    /// :rtype: None
    /// :raises OSError: if an error happens during the quad insertion.
    ///
    /// >>> store = Store()
    /// >>> store.add(Quad(NamedNode('http://example.com'), NamedNode('http://example.com/p'), Literal('1'), NamedNode('http://example.com/g')))
    /// >>> list(store)
    /// [<Quad subject=<NamedNode value=http://example.com> predicate=<NamedNode value=http://example.com/p> object=<Literal value=1 datatype=<NamedNode value=http://www.w3.org/2001/XMLSchema#string>> graph_name=<NamedNode value=http://example.com/g>>]
    fn add(&self, quad: &PyQuad, py: Python<'_>) -> PyResult<()> {
        py.detach(|| {
            self.inner
                .insert(&quad.clone().into())
                .map_err(map_anyhow_error)?;
            Ok(())
        })
    }

    /// Adds a set of quads to this store.
    ///
    /// Applied as a single batch via [`oxigraph_nova_store::Store::extend`]: the whole batch is
    /// durably logged and becomes visible to concurrent readers atomically (as a unit, rather
    /// than quad-by-quad), which is stronger than calling :py:meth:`add` in a loop. This is not,
    /// however, a full ACID transaction with in-process rollback: if an individual quad in the
    /// batch fails to apply, quads already applied earlier in the same call are not undone.
    ///
    /// :param quads: the quads to add.
    /// :type quads: collections.abc.Iterable[Quad]
    /// :rtype: None
    /// :raises OSError: if an error happens during the quad insertion.
    ///
    /// >>> store = Store()
    /// >>> store.extend([Quad(NamedNode('http://example.com'), NamedNode('http://example.com/p'), Literal('1'), NamedNode('http://example.com/g'))])
    /// >>> list(store)
    /// [<Quad subject=<NamedNode value=http://example.com> predicate=<NamedNode value=http://example.com/p> object=<Literal value=1 datatype=<NamedNode value=http://www.w3.org/2001/XMLSchema#string>> graph_name=<NamedNode value=http://example.com/g>>]
    fn extend(&self, quads: &Bound<'_, PyAny>, py: Python<'_>) -> PyResult<()> {
        let quads = quads
            .try_iter()?
            .map(|q| Ok(Quad::from(q?.extract::<PyQuad>()?)))
            .collect::<PyResult<Vec<Quad>>>()?;
        py.detach(|| {
            self.inner.extend(quads).map_err(map_anyhow_error)?;
            Ok(())
        })
    }

    /// Adds a set of quads to this store without keeping them all into memory.
    ///
    /// :param quads: the quads to add.
    /// :type quads: collections.abc.Iterable[Quad]
    /// :rtype: None
    /// :raises OSError: if an error happens during the quad insertion.
    ///
    /// >>> store = Store()
    /// >>> store.bulk_extend([Quad(NamedNode('http://example.com'), NamedNode('http://example.com/p'), Literal('1'), NamedNode('http://example.com/g'))])
    /// >>> list(store)
    /// [<Quad subject=<NamedNode value=http://example.com> predicate=<NamedNode value=http://example.com/p> object=<Literal value=1 datatype=<NamedNode value=http://www.w3.org/2001/XMLSchema#string>> graph_name=<NamedNode value=http://example.com/g>>]
    fn bulk_extend(&self, quads: &Bound<'_, PyAny>) -> PyResult<()> {
        let quads = quads
            .try_iter()?
            .map(|q| Ok(Quad::from(q?.extract::<PyQuad>()?)))
            .collect::<PyResult<Vec<Quad>>>()?;
        self.inner
            .bulk_loader()
            .load(quads)
            .map_err(map_anyhow_error)?;
        Ok(())
    }

    /// Removes a quad from the store.
    ///
    /// :param quad: the quad to remove.
    /// :type quad: Quad
    /// :rtype: None
    /// :raises OSError: if an error happens during the quad removal.
    ///
    /// >>> store = Store()
    /// >>> quad = Quad(NamedNode('http://example.com'), NamedNode('http://example.com/p'), Literal('1'), NamedNode('http://example.com/g'))
    /// >>> store.add(quad)
    /// >>> store.remove(quad)
    /// >>> list(store)
    /// []
    fn remove(&self, quad: &PyQuad, py: Python<'_>) -> PyResult<()> {
        py.detach(|| {
            self.inner
                .remove(&quad.clone().into())
                .map_err(map_anyhow_error)?;
            Ok(())
        })
    }

    /// Looks for the quads matching a given pattern.
    ///
    /// :param subject: the quad subject or :py:const:`None` to match everything.
    /// :type subject: NamedNode or BlankNode or Triple or None
    /// :param predicate: the quad predicate or :py:const:`None` to match everything.
    /// :type predicate: NamedNode or None
    /// :param object: the quad object or :py:const:`None` to match everything.
    /// :type object: NamedNode or BlankNode or Literal or Triple or None
    /// :param graph_name: the quad graph name. To match only the default graph, use :py:class:`DefaultGraph`. To match everything use :py:const:`None`.
    /// :type graph_name: NamedNode or BlankNode or DefaultGraph or None, optional
    /// :return: an iterator of the quads matching the pattern.
    /// :rtype: collections.abc.Iterator[Quad]
    /// :raises OSError: if an error happens during the quads lookup.
    ///
    /// >>> store = Store()
    /// >>> store.add(Quad(NamedNode('http://example.com'), NamedNode('http://example.com/p'), Literal('1'), NamedNode('http://example.com/g')))
    /// >>> list(store.quads_for_pattern(NamedNode('http://example.com'), None, None, None))
    /// [<Quad subject=<NamedNode value=http://example.com> predicate=<NamedNode value=http://example.com/p> object=<Literal value=1 datatype=<NamedNode value=http://www.w3.org/2001/XMLSchema#string>> graph_name=<NamedNode value=http://example.com/g>>]
    #[pyo3(signature = (subject, predicate, object, graph_name = None))]
    fn quads_for_pattern(
        &self,
        subject: Option<PyNamedOrBlankNode>,
        predicate: Option<PyNamedNode>,
        object: Option<PyTerm>,
        graph_name: Option<PyGraphName>,
    ) -> PyResult<QuadIter> {
        let subject: Option<Term> = subject.map(|s| Term::from(NamedOrBlankNode::from(s)));
        let predicate = predicate.map(oxigraph_nova_core::NamedNode::from);
        let object: Option<Term> = object.map(Term::from);
        let graph_name: Option<GraphName> = graph_name.map(GraphName::from);
        let quads: Vec<PyQuad> = self
            .inner
            .quads_for_pattern(
                subject.as_ref(),
                predicate.as_ref(),
                object.as_ref(),
                graph_name.as_ref(),
            )
            .map_err(map_anyhow_error)?
            .filter_map(|sq| {
                let sq = match sq {
                    Ok(sq) => sq,
                    Err(e) => return Some(Err(map_anyhow_error(e))),
                };
                let subject = match sq.subject.as_ref() {
                    Term::NamedNode(n) => NamedOrBlankNode::NamedNode(n.clone()),
                    Term::BlankNode(b) => NamedOrBlankNode::BlankNode(b.clone()),
                    _ => return None, // quoted-triple subjects can't round-trip through oxrdf::Quad
                };
                let object = sq.object.as_ref().clone();
                Some(Ok(PyQuad::from(Quad::new(
                    subject,
                    sq.predicate,
                    object,
                    sq.graph_name,
                ))))
            })
            .collect::<PyResult<Vec<_>>>()?;
        Ok(QuadIter {
            inner: quads.into_iter(),
        })
    }

    /// Executes a `SPARQL 1.1 query <https://www.w3.org/TR/sparql11-query/>`_.
    ///
    /// Only the ``base_iri`` and ``prefixes`` parameters are supported by this Nova-backed store;
    /// custom-function, dataset-selection, and substitution parameters are not yet implemented.
    ///
    /// :param query: the query to execute.
    /// :type query: str
    /// :param base_iri: the base IRI used to resolve the relative IRIs in the SPARQL query or :py:const:`None` if relative IRI resolution should not be done.
    /// :type base_iri: str or None, optional
    /// :param prefixes: a set of default prefixes to use during the SPARQL query parsing as a prefix name -> prefix IRI dictionary.
    /// :type prefixes: dict[str, str] or None, optional
    /// :return: a :py:class:`bool` for ``ASK`` queries, an iterator of :py:class:`Triple` for ``CONSTRUCT`` and ``DESCRIBE`` queries and an iterator of :py:class:`QuerySolution` for ``SELECT`` queries.
    /// :rtype: QuerySolutions or QueryBoolean or QueryTriples
    /// :raises SyntaxError: if the provided query is invalid.
    /// :raises OSError: if an error happens while reading the store.
    ///
    /// ``SELECT`` query:
    ///
    /// >>> store = Store()
    /// >>> store.add(Quad(NamedNode('http://example.com'), NamedNode('http://example.com/p'), Literal('1')))
    /// >>> [solution['s'] for solution in store.query('SELECT ?s WHERE { ?s ?p ?o }')]
    /// [<NamedNode value=http://example.com>]
    #[expect(clippy::too_many_arguments, clippy::doc_link_with_quotes)]
    #[pyo3(signature = (query, *, base_iri = None, prefixes = None, use_default_graph_as_union = false, default_graph = None, named_graphs = None, substitutions = None, custom_functions = None, custom_aggregate_functions = None))]
    fn query<'py>(
        &self,
        query: &str,
        base_iri: Option<&str>,
        prefixes: Option<HashMap<String, String>>,
        use_default_graph_as_union: bool,
        default_graph: Option<&Bound<'_, PyAny>>,
        named_graphs: Option<&Bound<'_, PyAny>>,
        substitutions: Option<HashMap<PyVariable, PyTerm>>,
        custom_functions: Option<HashMap<PyNamedNode, Py<PyAny>>>,
        custom_aggregate_functions: Option<HashMap<PyNamedNode, Py<PyAny>>>,
        py: Python<'py>,
    ) -> PyResult<Bound<'py, PyAny>> {
        if use_default_graph_as_union
            || default_graph.is_some()
            || named_graphs.is_some()
            || substitutions.is_some()
            || custom_functions.is_some()
            || custom_aggregate_functions.is_some()
        {
            return Err(PyValueError::new_err(
                "use_default_graph_as_union, default_graph, named_graphs, substitutions, \
                 custom_functions and custom_aggregate_functions are not supported by this \
                 Nova-backed store",
            ));
        }

        if base_iri.is_none() && prefixes.is_none() {
            let (results, vars) = py
                .detach(|| {
                    self.inner
                        .query_with_variables(query, QueryOptions::default())
                })
                .map_err(map_anyhow_error)?;
            return query_results_to_python(py, results, vars);
        }

        let mut parser = SparqlParser::new();
        if let Some(base_iri) = base_iri {
            parser = parser
                .with_base_iri(base_iri)
                .map_err(|e| PyValueError::new_err(e.to_string()))?;
        }
        if let Some(prefixes) = prefixes {
            for (name, iri) in prefixes {
                parser = parser
                    .with_prefix(name, iri)
                    .map_err(|e| PyValueError::new_err(e.to_string()))?;
            }
        }
        let parsed = parser.parse_query(query).map_err(map_sparql_syntax_error)?;
        let vars = oxigraph_nova_query::projected_variables(&parsed);
        let dataset = oxigraph_nova_query::StoreDataset::new(self.inner.inner());

        let evaluator =
            oxigraph_nova_query::Evaluator::with_options(&dataset, QueryOptions::default());
        // Not wrapped in `py.detach`: `Evaluator` holds a `RefCell` for its base-IRI
        // cache, so it isn't `Sync`/`Send` and can't cross the GIL-release boundary.
        // This custom base_iri/prefixes path is the less common one; the fast path
        // above (no custom parser options) still releases the GIL.
        let result = evaluator.evaluate(&parsed).map_err(map_anyhow_error)?;
        query_results_to_python(
            py,
            oxigraph_nova_store::collect_query_result(result).map_err(map_anyhow_error)?,
            vars,
        )
    }

    /// Executes a `SPARQL 1.1 update <https://www.w3.org/TR/sparql11-update/>`_.
    ///
    /// Only the ``base_iri`` and ``prefixes`` parameters are supported by this Nova-backed store;
    /// custom-function parameters are not yet implemented.
    ///
    /// :param update: the update to execute.
    /// :type update: str
    /// :param base_iri: the base IRI used to resolve the relative IRIs in the SPARQL update or :py:const:`None` if relative IRI resolution should not be done.
    /// :type base_iri: str or None, optional
    /// :param prefixes: a set of default prefixes to use during the SPARQL query parsing as a prefix name -> prefix IRI dictionary.
    /// :type prefixes: dict[str, str] or None, optional
    /// :rtype: None
    /// :raises SyntaxError: if the provided update is invalid.
    /// :raises OSError: if an error happens while reading the store.
    ///
    /// ``INSERT DATA`` update:
    ///
    /// >>> store = Store()
    /// >>> store.update('INSERT DATA { <http://example.com> <http://example.com/p> "1" }')
    /// >>> list(store)
    /// [<Quad subject=<NamedNode value=http://example.com> predicate=<NamedNode value=http://example.com/p> object=<Literal value=1 datatype=<NamedNode value=http://www.w3.org/2001/XMLSchema#string>> graph_name=<DefaultGraph>>]
    #[pyo3(signature = (update, *, base_iri = None, prefixes = None, custom_functions = None, custom_aggregate_functions = None))]
    fn update(
        &self,
        update: &str,
        base_iri: Option<&str>,
        prefixes: Option<HashMap<String, String>>,
        custom_functions: Option<HashMap<PyNamedNode, Py<PyAny>>>,
        custom_aggregate_functions: Option<HashMap<PyNamedNode, Py<PyAny>>>,
        py: Python<'_>,
    ) -> PyResult<()> {
        if custom_functions.is_some() || custom_aggregate_functions.is_some() {
            return Err(PyValueError::new_err(
                "custom_functions and custom_aggregate_functions are not supported by this \
                 Nova-backed store",
            ));
        }
        py.detach(|| {
            if base_iri.is_none() && prefixes.is_none() {
                self.inner.update(update).map_err(map_anyhow_error)
            } else {
                let mut parser = SparqlParser::new();
                if let Some(base_iri) = base_iri {
                    parser = parser
                        .with_base_iri(base_iri)
                        .map_err(|e| PyValueError::new_err(e.to_string()))?;
                }
                if let Some(prefixes) = prefixes {
                    for (name, iri) in prefixes {
                        parser = parser
                            .with_prefix(name, iri)
                            .map_err(|e| PyValueError::new_err(e.to_string()))?;
                    }
                }
                let parsed = parser
                    .parse_update(update)
                    .map_err(map_sparql_syntax_error)?;
                oxigraph_nova_query::execute_update(&self.inner.inner(), &parsed)
                    .map_err(map_anyhow_error)
            }
        })
    }

    /// Loads RDF serialization into the store.
    ///
    /// Beware, the full file is loaded into memory.
    ///
    /// :param input: The :py:class:`str`, :py:class:`bytes` or I/O object to read from.
    /// :type input: bytes or str or typing.IO[bytes] or typing.IO[str] or None, optional
    /// :param format: the format of the RDF serialization. If :py:const:`None`, the format is guessed from the file name extension.
    /// :type format: RdfFormat or None, optional
    /// :param path: The file path to read from. Replace the ``input`` parameter.
    /// :type path: str or os.PathLike[str] or None, optional
    /// :param base_iri: the base IRI used to resolve the relative IRIs in the file or :py:const:`None` if relative IRI resolution should not be done.
    /// :type base_iri: str or None, optional
    /// :param to_graph: if it is a file composed of triples, the graph in which the triples should be stored. By default, the default graph is used.
    /// :type to_graph: NamedNode or BlankNode or DefaultGraph or None, optional
    /// :param lenient: not supported by this Nova-backed store; must be left :py:const:`False`.
    /// :type lenient: bool, optional
    /// :rtype: None
    /// :raises ValueError: if the format is not supported, or ``lenient`` is set.
    /// :raises SyntaxError: if the provided data is invalid.
    /// :raises OSError: if an error happens during a quad insertion or if a system error happens while reading the file.
    #[pyo3(signature = (input = None, format = None, *, path = None, base_iri = None, to_graph = None, lenient = false))]
    fn load(
        &self,
        input: Option<PyReadableInput>,
        format: Option<PyRdfFormat>,
        path: Option<PathBuf>,
        base_iri: Option<&str>,
        to_graph: Option<PyGraphName>,
        lenient: bool,
        py: Python<'_>,
    ) -> PyResult<()> {
        if lenient {
            return Err(PyValueError::new_err(
                "lenient loading is not supported by this Nova-backed store",
            ));
        }
        let format = lookup_rdf_format(format, path.as_deref())?;
        let to_graph: Option<GraphName> = to_graph.map(GraphName::from);
        py.detach(|| {
            if let Some(input) = input {
                match input {
                    PyReadableInput::Bytes(bytes) => {
                        self.inner
                            .load(&*bytes as &[u8], format, base_iri, to_graph.as_ref())
                    }
                    PyReadableInput::String(str) => {
                        self.inner
                            .load(str.as_bytes(), format, base_iri, to_graph.as_ref())
                    }
                    PyReadableInput::Io(io) => {
                        self.inner
                            .load(PyIo::new(io), format, base_iri, to_graph.as_ref())
                    }
                }
                .map_err(map_anyhow_error)?;
                Ok(())
            } else if let Some(path) = path {
                let file = File::open(&path)?;
                self.inner
                    .load(file, format, base_iri, to_graph.as_ref())
                    .map_err(map_anyhow_error)?;
                Ok(())
            } else {
                Err(PyValueError::new_err(
                    "Either input or file_path must be set",
                ))
            }
        })
    }

    /// Loads some RDF serialization into the store without keeping it all into memory.
    ///
    /// This Nova-backed store implements ``bulk_load`` identically to :py:func:`load`
    /// (there is no separate streaming/bulk code path at this facade level).
    #[pyo3(signature = (input = None, format = None, *, path = None, base_iri = None, to_graph = None, lenient = false))]
    fn bulk_load(
        &self,
        input: Option<PyReadableInput>,
        format: Option<PyRdfFormat>,
        path: Option<PathBuf>,
        base_iri: Option<&str>,
        to_graph: Option<PyGraphName>,
        lenient: bool,
        py: Python<'_>,
    ) -> PyResult<()> {
        self.load(input, format, path, base_iri, to_graph, lenient, py)
    }

    /// Dumps the store quads or triples into a file.
    ///
    /// ``prefixes`` and ``base_iri`` are not supported by this Nova-backed store's dump path.
    ///
    /// :param output: The binary I/O object or file path to write to. If :py:const:`None`, a :py:class:`bytes` buffer is returned with the serialized content.
    /// :type output: typing.IO[bytes] or str or os.PathLike[str] or None, optional
    /// :param format: the format of the RDF serialization. If :py:const:`None`, the format is guessed from the file name extension.
    /// :type format: RdfFormat or None, optional
    /// :param from_graph: the store graph from which dump the triples. Required if the serialization format does not support named graphs.
    /// :type from_graph: NamedNode or BlankNode or DefaultGraph or None, optional
    /// :return: :py:class:`bytes` with the serialization if the ``output`` parameter is :py:const:`None`, :py:const:`None` if ``output`` is set.
    /// :rtype: bytes or None
    /// :raises ValueError: if the format is not supported, the `from_graph` parameter is not given with a syntax not supporting named graphs, or `prefixes`/`base_iri` are given.
    /// :raises OSError: if an error happens during a quad lookup or file writing.
    #[pyo3(signature = (output = None, format = None, *, from_graph = None, prefixes = None, base_iri = None))]
    fn dump(
        &self,
        output: Option<PyWritableOutput>,
        format: Option<PyRdfFormat>,
        from_graph: Option<PyGraphName>,
        prefixes: Option<BTreeMap<String, String>>,
        base_iri: Option<&str>,
        py: Python<'_>,
    ) -> PyResult<Option<Vec<u8>>> {
        if prefixes.is_some() || base_iri.is_some() {
            return Err(PyValueError::new_err(
                "prefixes and base_iri are not supported by this Nova-backed store's dump()",
            ));
        }
        let from_graph: Option<GraphName> = from_graph.map(GraphName::from);
        PyWritable::do_write(
            |mut output, file_path| {
                py.detach(|| {
                    let format = lookup_rdf_format(format, file_path.as_deref())?;
                    self.inner
                        .dump(&mut output, format, from_graph.as_ref())
                        .map_err(map_anyhow_error)?;
                    Ok(output)
                })
            },
            output,
            py,
        )
    }

    /// Returns an iterator over all the store named graphs.
    ///
    /// :return: an iterator of the store graph names.
    /// :rtype: collections.abc.Iterator[NamedNode or BlankNode]
    /// :raises OSError: if an error happens during the named graphs lookup.
    ///
    /// >>> store = Store()
    /// >>> store.add(Quad(NamedNode('http://example.com'), NamedNode('http://example.com/p'), Literal('1'), NamedNode('http://example.com/g')))
    /// >>> list(store.named_graphs())
    /// [<NamedNode value=http://example.com/g>]
    fn named_graphs(&self) -> PyResult<GraphNameIter> {
        let graphs = self.inner.named_graphs().map_err(map_anyhow_error)?;
        let graphs: Vec<PyNamedOrBlankNode> = graphs
            .into_iter()
            .filter_map(|g| match g {
                GraphName::NamedNode(n) => Some(PyNamedOrBlankNode::NamedNode(n.into())),
                GraphName::BlankNode(b) => Some(PyNamedOrBlankNode::BlankNode(b.into())),
                GraphName::DefaultGraph => None,
            })
            .collect();
        Ok(GraphNameIter {
            inner: graphs.into_iter(),
        })
    }

    /// Returns if the store contains the given named graph.
    ///
    /// :param graph_name: the name of the named graph.
    /// :type graph_name: NamedNode or BlankNode or DefaultGraph
    /// :rtype: bool
    /// :raises OSError: if an error happens during the named graph lookup.
    ///
    /// >>> store = Store()
    /// >>> store.add_graph(NamedNode('http://example.com/g'))
    /// >>> store.contains_named_graph(NamedNode('http://example.com/g'))
    /// True
    fn contains_named_graph(&self, graph_name: PyGraphName) -> PyResult<bool> {
        let graph_name: GraphName = graph_name.into();
        self.inner
            .contains_named_graph(&graph_name)
            .map_err(map_anyhow_error)
    }

    /// Adds a named graph to the store.
    ///
    /// Blank-node-named graphs cannot be created through this Nova-backed store (SPARQL Update's
    /// ``CREATE GRAPH`` clause requires an IRI).
    ///
    /// :param graph_name: the name of the name graph to add.
    /// :type graph_name: NamedNode or BlankNode or DefaultGraph
    /// :rtype: None
    /// :raises OSError: if an error happens during the named graph insertion.
    ///
    /// >>> store = Store()
    /// >>> store.add_graph(NamedNode('http://example.com/g'))
    /// >>> list(store.named_graphs())
    /// [<NamedNode value=http://example.com/g>]
    fn add_graph(&self, graph_name: PyGraphName, py: Python<'_>) -> PyResult<()> {
        py.detach(|| match GraphName::from(graph_name) {
            GraphName::DefaultGraph => Ok(()),
            GraphName::NamedNode(n) => self
                .inner
                .update(&format!("CREATE SILENT GRAPH <{}>", n.as_str()))
                .map_err(map_anyhow_error),
            GraphName::BlankNode(_) => Err(PyValueError::new_err(
                "blank-node-named graphs cannot be created by this Nova-backed store",
            )),
        })
    }

    /// Clears a graph from the store without removing it.
    ///
    /// :param graph_name: the name of the name graph to clear.
    /// :type graph_name: NamedNode or BlankNode or DefaultGraph
    /// :rtype: None
    /// :raises OSError: if an error happens during the operation.
    fn clear_graph(&self, graph_name: PyGraphName, py: Python<'_>) -> PyResult<()> {
        py.detach(|| match GraphName::from(graph_name) {
            GraphName::DefaultGraph => {
                self.inner.update("CLEAR DEFAULT").map_err(map_anyhow_error)
            }
            GraphName::NamedNode(n) => self
                .inner
                .update(&format!("CLEAR SILENT GRAPH <{}>", n.as_str()))
                .map_err(map_anyhow_error),
            GraphName::BlankNode(_) => Err(PyValueError::new_err(
                "blank-node-named graphs are not supported by this Nova-backed store's clear_graph()",
            )),
        })
    }

    /// Removes a graph from the store.
    ///
    /// The default graph will not be removed but just cleared.
    ///
    /// :param graph_name: the name of the name graph to remove.
    /// :type graph_name: NamedNode or BlankNode or DefaultGraph
    /// :rtype: None
    /// :raises OSError: if an error happens during the named graph removal.
    fn remove_graph(&self, graph_name: PyGraphName, py: Python<'_>) -> PyResult<()> {
        py.detach(|| match GraphName::from(graph_name) {
            GraphName::DefaultGraph => {
                self.inner.update("CLEAR DEFAULT").map_err(map_anyhow_error)
            }
            GraphName::NamedNode(n) => self
                .inner
                .update(&format!("DROP SILENT GRAPH <{}>", n.as_str()))
                .map_err(map_anyhow_error),
            GraphName::BlankNode(_) => Err(PyValueError::new_err(
                "blank-node-named graphs are not supported by this Nova-backed store's remove_graph()",
            )),
        })
    }

    /// Clears the store by removing all its contents.
    ///
    /// :rtype: None
    /// :raises OSError: if an error happens during the operation.
    fn clear(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.inner.update("CLEAR ALL").map_err(map_anyhow_error))
    }

    /// Flushes all buffers and ensures that all writes are saved on disk.
    ///
    /// This is a no-op on this Nova-backed store: writes are durable as soon as they are
    /// acknowledged (via the write-ahead log), so there is no separate flush step to perform.
    ///
    /// :rtype: None
    fn flush(&self) -> PyResult<()> {
        Ok(())
    }

    /// Optimizes the database for future workload.
    ///
    /// Useful to call after a batch upload or another similar operation.
    ///
    /// :rtype: None
    /// :raises OSError: if an error happens during the optimization.
    fn optimize(&self, py: Python<'_>) -> PyResult<()> {
        py.detach(|| self.inner.optimize().map_err(map_anyhow_error))
    }

    /// Creates database backup into the `target_directory`.
    ///
    /// :param target_directory: the directory name to save the database to.
    /// :type target_directory: str or os.PathLike[str]
    /// :rtype: None
    /// :raises OSError: if an error happens during the backup.
    fn backup(&self, target_directory: PathBuf, py: Python<'_>) -> PyResult<()> {
        py.detach(|| {
            self.inner
                .backup(target_directory)
                .map_err(map_anyhow_error)
        })
    }

    fn __bool__(&self) -> PyResult<bool> {
        Ok(!self.inner.is_empty().map_err(map_anyhow_error)?)
    }

    fn __len__(&self) -> PyResult<usize> {
        self.inner.len().map_err(map_anyhow_error)
    }

    fn __contains__(&self, quad: &PyQuad) -> PyResult<bool> {
        self.inner
            .contains(&quad.clone().into())
            .map_err(map_anyhow_error)
    }

    fn __iter__(&self) -> PyResult<QuadIter> {
        self.quads_for_pattern(None, None, None, None)
    }
}

impl fmt::Display for PyStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buffer = Vec::new();
        if self
            .inner
            .dump(&mut buffer, oxrdfio::RdfFormat::NQuads, None)
            .is_err()
        {
            return Err(fmt::Error);
        }
        f.write_str(&String::from_utf8_lossy(&buffer))
    }
}

#[pyclass(module = "pyoxigraph")]
pub struct QuadIter {
    inner: std::vec::IntoIter<PyQuad>,
}

#[pymethods]
impl QuadIter {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self) -> Option<PyQuad> {
        self.inner.next()
    }
}

#[pyclass(module = "pyoxigraph")]
pub struct GraphNameIter {
    inner: std::vec::IntoIter<PyNamedOrBlankNode>,
}

#[pymethods]
impl GraphNameIter {
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __next__(&mut self) -> Option<PyNamedOrBlankNode> {
        self.inner.next()
    }
}

/// Extracts the original `PyErr` from a `std::io::Error`, if it was constructed by pyo3's
/// blanket `impl From<PyErr> for io::Error` (i.e. it wraps a Python exception raised by a
/// `PyIo`/`PyReadable` callback such as calling `.read()` on a write-only file).
fn extract_py_err_from_io_error(io_err: &std::io::Error) -> Option<&PyErr> {
    io_err.get_ref().and_then(|inner| inner.downcast_ref())
}

/// Maps any error from `oxigraph-nova-store`/`oxigraph-nova-query` (all surfaced as
/// `anyhow::Error`) to a Python exception. This Nova-backed store does not distinguish
/// storage/loader/serializer error subtypes the way upstream `oxigraph::store::Store` does,
/// with one exception: if the error chain contains a `std::io::Error` wrapping a Python
/// exception raised by a `PyIo`/`PyReadable` callback (e.g. calling `.read()` on a file
/// opened in write-only mode raises `io.UnsupportedOperation`), that original Python
/// exception is re-raised as-is instead of being flattened into a generic `RuntimeError`
/// string — this matters because callers (including upstream pyoxigraph's test suite)
/// distinguish `OSError` subclasses like `io.UnsupportedOperation` by type, not by message.
///
/// Note: `oxrdfio::RdfParseError::Io` wraps its `io::Error` with `#[error(transparent)]`,
/// which makes thiserror forward `source()` straight through to the *inner* io::Error's own
/// source (skipping the io::Error itself), so `anyhow::Error::chain()` never yields the
/// io::Error as a separate item to downcast. The `RdfParseError` variant has to be matched
/// directly to reach the wrapped `io::Error`.
pub fn map_anyhow_error(e: anyhow::Error) -> PyErr {
    for cause in e.chain() {
        if let Some(io_err) = cause.downcast_ref::<std::io::Error>()
            && let Some(py_err) = extract_py_err_from_io_error(io_err)
        {
            return Python::attach(|py| py_err.clone_ref(py));
        }
        if let Some(oxrdfio::RdfParseError::Io(io_err)) =
            cause.downcast_ref::<oxrdfio::RdfParseError>()
            && let Some(py_err) = extract_py_err_from_io_error(io_err)
        {
            return Python::attach(|py| py_err.clone_ref(py));
        }
    }
    PyRuntimeError::new_err(e.to_string())
}
