use crate::format_err;
use crate::io::{BytesInput, buffer_from_js_value, convert_base_iri, rdf_format};
use crate::model::*;
use crate::reflect::*;
use crate::utils::{to_option, to_option_ref};
use js_sys::{Array, Map, try_iter};
use oxigraph_nova_query::{
    Evaluator, QueryOptions, StoreDataset, execute_update, projected_variables,
};
use oxigraph_nova_store::{QueryResults, Store};
use oxrdf::{NamedOrBlankNode, Quad, Term};
use oxrdfio::{RdfParser, RdfSerializer};
use sparesults::{QueryResultsFormat, QueryResultsSerializer};
use spargebra::SparqlParser;
use wasm_bindgen::prelude::*;

#[wasm_bindgen(js_name = Store, skip_typescript)]
pub struct JsStore {
    store: Store,
    data_factory: DataFactory,
}

#[wasm_bindgen(js_class = Store)]
impl JsStore {
    #[wasm_bindgen(constructor)]
    pub fn new(quads: &JsValue) -> Result<JsStore, JsValue> {
        console_error_panic_hook::set_once();
        let store = Self {
            store: Store::new(),
            data_factory: default_data_factory(),
        };
        if let Some(quads) = to_option_ref(quads)
            && let Some(quads) = try_iter(quads)?
        {
            for quad in quads {
                store.add(&quad?)?;
            }
        }
        Ok(store)
    }

    pub fn add(&self, quad: &JsValue) -> Result<(), JsValue> {
        self.store
            .insert(&to_quad(quad)?)
            .map_err(map_anyhow_error)?;
        Ok(())
    }

    pub fn delete(&self, quad: &JsValue) -> Result<(), JsValue> {
        self.store
            .remove(&to_quad(quad)?)
            .map_err(map_anyhow_error)?;
        Ok(())
    }

    pub fn has(&self, quad: &JsValue) -> Result<bool, JsValue> {
        self.store
            .contains(&to_quad(quad)?)
            .map_err(map_anyhow_error)
    }

    #[wasm_bindgen(getter = size)]
    pub fn size(&self) -> Result<usize, JsValue> {
        self.store.len().map_err(map_anyhow_error)
    }

    #[wasm_bindgen(js_name = match)]
    pub fn match_quads(
        &self,
        subject: &JsValue,
        predicate: &JsValue,
        object: &JsValue,
        graph_name: &JsValue,
    ) -> Result<Vec<JsValue>, JsValue> {
        let subject = to_option_ref(subject)
            .map(to_named_or_blank_node)
            .transpose()?
            .map(Term::from);
        let predicate = to_option_ref(predicate).map(to_named_node).transpose()?;
        let object = to_option_ref(object).map(to_term).transpose()?;
        let graph_name = to_option_ref(graph_name).map(to_graph_name).transpose()?;
        let quads = self
            .store
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
                    // quoted-triple subjects can't round-trip through oxrdf::Quad
                    _ => return None,
                };
                let object = sq.object.as_ref().clone();
                let quad = Quad::new(subject, sq.predicate, object, sq.graph_name);
                Some(Ok(from_quad(&self.data_factory, &quad)))
            })
            .collect::<Result<Vec<_>, JsValue>>()?;
        Ok(quads)
    }

    pub fn query(&self, query: &str, options: &JsValue) -> Result<JsValue, JsValue> {
        let mut base_iri = None;
        let mut use_default_graph_as_union = false;
        let mut results_format = None;
        let mut default_graph = None;
        let mut named_graphs = None;
        if let Some(options) = to_option_ref(options) {
            base_iri = convert_base_iri(&reflect_get(options, &BASE_IRI)?)?;
            default_graph =
                if let Some(default_graph) = to_option(reflect_get(options, &DEFAULT_GRAPH)?) {
                    Some(if let Some(iter) = try_iter(&default_graph)? {
                        iter.map(|term| to_graph_name(&term?))
                            .collect::<Result<Vec<_>, _>>()?
                    } else {
                        vec![to_graph_name(&default_graph)?]
                    })
                } else {
                    None
                };
            named_graphs =
                if let Some(named_graphs) = to_option(reflect_get(options, &NAMED_GRAPHS)?) {
                    Some(
                        try_iter(&named_graphs)?
                            .ok_or_else(|| format_err!("named_graphs option must be iterable"))?
                            .map(|term| to_named_or_blank_node(&term?))
                            .collect::<Result<Vec<_>, _>>()?,
                    )
                } else {
                    None
                };
            use_default_graph_as_union =
                reflect_get(options, &USED_DEFAULT_GRAPH_AS_UNION)?.is_truthy();
            if let Some(js_results_format) = to_option(reflect_get(options, &RESULTS_FORMAT)?) {
                results_format = Some(
                    js_results_format
                        .as_string()
                        .ok_or_else(|| format_err!("results_format option must be a string"))?,
                );
            }
        }

        if use_default_graph_as_union || default_graph.is_some() || named_graphs.is_some() {
            return Err(format_err!(
                "default_graph, named_graphs and use_default_graph_as_union are not supported by \
                 this Nova-backed store's query()"
            ));
        }

        let (results, vars) = if let Some(base_iri) = base_iri {
            let parsed = SparqlParser::new()
                .with_base_iri(base_iri)
                .map_err(JsError::from)?
                .parse_query(query)
                .map_err(JsError::from)?;
            let vars = projected_variables(&parsed);
            let dataset = StoreDataset::new(self.store.engine());
            let evaluator = Evaluator::with_options(&dataset, QueryOptions::default());
            let result = evaluator.evaluate(&parsed).map_err(map_anyhow_error)?;
            (
                oxigraph_nova_store::collect_query_result(result).map_err(map_anyhow_error)?,
                vars,
            )
        } else {
            self.store
                .query_with_variables(query, QueryOptions::default())
                .map_err(map_anyhow_error)?
        };

        Ok(match results {
            QueryResults::Solutions(solutions) => {
                if let Some(results_format) = results_format {
                    let mut serializer =
                        QueryResultsSerializer::from_format(query_results_format(&results_format)?)
                            .serialize_solutions_to_writer(Vec::new(), vars.clone())
                            .map_err(JsError::from)?;
                    for solution in &solutions {
                        serializer
                            .serialize(vars.iter().filter_map(|v| solution.get(v).map(|t| (v, t))))
                            .map_err(JsError::from)?;
                    }
                    JsValue::from_str(
                        &String::from_utf8(serializer.finish().map_err(JsError::from)?)
                            .map_err(JsError::from)?,
                    )
                } else {
                    let results = Array::new();
                    for solution in &solutions {
                        let result = Map::new();
                        for (variable, value) in solution.iter() {
                            result.set(
                                &variable.as_str().into(),
                                &from_term(&self.data_factory, value),
                            );
                        }
                        results.push(&result.into());
                    }
                    results.into()
                }
            }
            QueryResults::Graph(triples) => {
                if let Some(results_format) = results_format {
                    let mut serializer = RdfSerializer::from_format(rdf_format(&results_format)?)
                        .for_writer(Vec::new());
                    for triple in &triples {
                        serializer.serialize_triple(triple).map_err(JsError::from)?;
                    }
                    JsValue::from_str(
                        &String::from_utf8(serializer.finish().map_err(JsError::from)?)
                            .map_err(JsError::from)?,
                    )
                } else {
                    let results = Array::new();
                    for triple in &triples {
                        results.push(&from_triple(&self.data_factory, triple));
                    }
                    results.into()
                }
            }
            QueryResults::Boolean(b) => {
                if let Some(results_format) = results_format {
                    JsValue::from_str(
                        &String::from_utf8(
                            QueryResultsSerializer::from_format(query_results_format(
                                &results_format,
                            )?)
                            .serialize_boolean_to_writer(Vec::new(), b)
                            .map_err(JsError::from)?,
                        )
                        .map_err(JsError::from)?,
                    )
                } else {
                    b.into()
                }
            }
        })
    }

    pub fn update(&self, update: &str, options: &JsValue) -> Result<(), JsValue> {
        let mut base_iri = None;
        if let Some(options) = to_option_ref(options) {
            base_iri = convert_base_iri(&reflect_get(options, &BASE_IRI)?)?;
        }
        if let Some(base_iri) = base_iri {
            let parsed = SparqlParser::new()
                .with_base_iri(base_iri)
                .map_err(JsError::from)?
                .parse_update(update)
                .map_err(JsError::from)?;
            execute_update(&self.store.engine(), &parsed).map_err(map_anyhow_error)?;
        } else {
            self.store.update(update).map_err(map_anyhow_error)?;
        }
        Ok(())
    }

    pub fn load(&self, data: &JsValue, options: &JsValue) -> Result<(), JsValue> {
        let mut format = None;
        let mut base_iri = None;
        let mut to_graph_name_rs = None;
        let mut lenient = false;
        let mut no_transaction = false;
        if let Some(options) = to_option_ref(options) {
            if let Some(format_str) = reflect_get(options, &FORMAT)?.as_string() {
                format = Some(rdf_format(&format_str)?);
            }
            base_iri = convert_base_iri(&reflect_get(options, &BASE_IRI)?)?;
            to_graph_name_rs = to_option_ref(&reflect_get(options, &TO_GRAPH_NAME)?)
                .map(to_graph_name)
                .transpose()?;
            lenient = reflect_get(options, &LENIENT)?.is_truthy();
            no_transaction = reflect_get(options, &NO_TRANSACTION)?.is_truthy();
        }
        let format = format.ok_or_else(|| {
            format_err!(
                "The format option should be provided as a second argument of Store.load like \
                 store.load(my_content, {{format: 'nt'}}"
            )
        })?;

        let mut parser = RdfParser::from_format(format);
        if let Some(to_graph_name) = to_graph_name_rs {
            parser = parser.with_default_graph(to_graph_name);
        }
        if let Some(base_iri) = &base_iri {
            parser = parser.with_base_iri(base_iri).map_err(JsError::from)?;
        }
        if lenient {
            parser = parser.lenient();
        }

        // Build a `Vec<u8>` regardless of the input shape (string / Uint8Array /
        // iterator of chunks) so the same parse-then-insert code path below can
        // be shared by both the transactional and bulk-load branches. Nova's
        // `Store::load` facade doesn't expose `lenient`, so we bypass it here
        // and drive `RdfParser`/`self.store.engine()` (the raw `Arc<dyn StorageEngine>`,
        // which implements `QuadStore`) directly, matching `Store::load`'s own
        // internal one-quad-at-a-time insert loop for the transactional path.
        let quads: Vec<Quad> = if let Some(buffer) = buffer_from_js_value(data) {
            parser
                .for_slice(&buffer)
                .collect::<Result<Vec<_>, _>>()
                .map_err(JsError::from)?
        } else if let Some(iterator) = try_iter(data)? {
            parser
                .for_reader(BytesInput::from(iterator))
                .collect::<Result<Vec<_>, _>>()
                .map_err(JsError::from)?
        } else {
            return Err(format_err!(
                "The input must be a string, Uint8Array or an iterator of string or Uint8Array"
            ));
        };

        if no_transaction {
            self.store
                .bulk_loader()
                .load(quads)
                .map_err(map_anyhow_error)?;
        } else {
            let inner = self.store.engine();
            for quad in &quads {
                inner
                    .insert(quad)
                    .map_err(|e| map_anyhow_error(anyhow::anyhow!("{e}")))?;
            }
        }
        Ok(())
    }

    pub fn dump(&self, options: &JsValue) -> Result<String, JsValue> {
        let mut format = None;
        let mut from_graph_name_rs = None;
        if let Some(options) = to_option_ref(options) {
            if let Some(format_str) = reflect_get(options, &FORMAT)?.as_string() {
                format = Some(rdf_format(&format_str)?);
            }
            from_graph_name_rs = to_option_ref(&reflect_get(options, &FROM_GRAPH_NAME)?)
                .map(to_graph_name)
                .transpose()?;
        }
        let format = format.ok_or_else(|| {
            format_err!(
                "The format option should be provided as a second argument of Store.load like \
                 store.dump({{format: 'nt'}}"
            )
        })?;
        let mut buffer = Vec::new();
        self.store
            .dump(&mut buffer, format, from_graph_name_rs.as_ref())
            .map_err(map_anyhow_error)?;
        Ok(String::from_utf8(buffer).map_err(JsError::from)?)
    }
}

fn query_results_format(format: &str) -> Result<QueryResultsFormat, JsValue> {
    if format.contains('/') {
        QueryResultsFormat::from_media_type(format).ok_or_else(|| {
            format_err!(
                "Not supported SPARQL query results format media type: {}",
                format
            )
        })
    } else {
        QueryResultsFormat::from_extension(format).ok_or_else(|| {
            format_err!(
                "Not supported SPARQL query results format extension: {}",
                format
            )
        })
    }
}

fn map_anyhow_error(e: anyhow::Error) -> JsValue {
    JsError::new(&e.to_string()).into()
}
