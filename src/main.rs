use std::collections::BTreeMap;
use std::{
    fs::{self, ReadDir},
    path::{Path, PathBuf},
    vec,
};

use clap::Parser;
use lopdf::{Bookmark, Document, Object, ObjectId};

const DEFAULT_FILE_NAME: &str = "merged.pdf";

/// Merge PDF's in specified directory.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// The path to be searched
    #[arg(default_value_t = String::from("."))]
    inpath: String,

    /// The path of the output file
    #[arg(default_value_t = String::from(DEFAULT_FILE_NAME))]
    outpath: String,

    /// Annotate file names to corner of first slides
    #[arg(default_value_t = false, short, long)]
    anno: bool,
}

fn load_documents_from_path(path: &PathBuf) -> Vec<(Document, String)> {
    let mut docs: Vec<(Document, String)> = vec![];
    let dir: ReadDir;
    match fs::read_dir(path) {
        Ok(v) => dir = v,
        _ => return docs,
    }
    for entry in dir {
        if entry.is_err() {
            continue;
        }
        let entry = entry.unwrap();

        let filetype = entry.file_type();
        if filetype.is_err() {
            continue;
        }
        let filetype = filetype.unwrap();

        if filetype.is_file() {
            let file_path = entry.path();
            let file_name = entry.file_name();
            let file_name = file_name.to_str().unwrap_or("");

            if file_name.ends_with(".pdf") && file_name != DEFAULT_FILE_NAME {
                let doc = Document::load(file_path);
                if doc.is_err() {
                    continue;
                }
                docs.push((doc.unwrap(), file_name.to_string()));
            }
        } else if filetype.is_dir() {
            let recursive_docs = load_documents_from_path(&entry.path());
            for doc in recursive_docs {
                docs.push(doc);
            }
        }
    }
    return docs;
}

// code in merge stolen from library examples heheheha
fn merge(docs_with_names: Vec<(Document, String)>) -> Result<Document, &'static str> {
    // Define a starting `max_id` (will be used as start index for object_ids).
    let mut max_id = 1;
    let mut pagenum = 1;
    // Collect all Documents Objects grouped by a map
    let mut documents_pages = BTreeMap::new();
    let mut documents_objects = BTreeMap::new();
    let mut document = Document::with_version("1.5");

    for (mut doc, _) in docs_with_names {
        let mut first = false;
        doc.renumber_objects_with(max_id);

        max_id = doc.max_id + 1;

        documents_pages.extend(
            doc.get_pages()
                .into_iter()
                .map(|(_, object_id)| {
                    if !first {
                        let bookmark = Bookmark::new(
                            String::from(format!("Page_{}", pagenum)),
                            [0.0, 0.0, 1.0],
                            0,
                            object_id,
                        );
                        document.add_bookmark(bookmark, None);
                        first = true;
                        pagenum += 1;
                    }

                    (object_id, doc.get_object(object_id).unwrap().to_owned())
                })
                .collect::<BTreeMap<ObjectId, Object>>(),
        );
        documents_objects.extend(doc.objects);
    }

    // "Catalog" and "Pages" are mandatory.
    let mut catalog_object: Option<(ObjectId, Object)> = None;
    let mut pages_object: Option<(ObjectId, Object)> = None;

    // Process all objects except "Page" type
    for (object_id, object) in documents_objects.iter() {
        // We have to ignore "Page" (as are processed later), "Outlines" and "Outline" objects.
        // All other objects should be collected and inserted into the main Document.
        match object.type_name().unwrap_or("") {
            "Catalog" => {
                // Collect a first "Catalog" object and use it for the future "Pages".
                catalog_object = Some((
                    if let Some((id, _)) = catalog_object {
                        id
                    } else {
                        *object_id
                    },
                    object.clone(),
                ));
            }
            "Pages" => {
                // Collect and update a first "Pages" object and use it for the future "Catalog"
                // We have also to merge all dictionaries of the old and the new "Pages" object
                if let Ok(dictionary) = object.as_dict() {
                    let mut dictionary = dictionary.clone();
                    if let Some((_, ref object)) = pages_object {
                        if let Ok(old_dictionary) = object.as_dict() {
                            dictionary.extend(old_dictionary);
                        }
                    }

                    pages_object = Some((
                        if let Some((id, _)) = pages_object {
                            id
                        } else {
                            *object_id
                        },
                        Object::Dictionary(dictionary),
                    ));
                }
            }
            "Page" => {}     // Ignored, processed later and separately
            "Outlines" => {} // Ignored, not supported yet
            "Outline" => {}  // Ignored, not supported yet
            _ => {
                document.objects.insert(*object_id, object.clone());
            }
        }
    }

    // If no "Pages" object found, abort.
    if pages_object.is_none() {
        return Err("Pages root not found");
    }

    // Iterate over all "Page" objects and collect into the parent "Pages" created before
    for (object_id, object) in documents_pages.iter() {
        if let Ok(dictionary) = object.as_dict() {
            let mut dictionary = dictionary.clone();
            dictionary.set("Parent", pages_object.as_ref().unwrap().0);

            document
                .objects
                .insert(*object_id, Object::Dictionary(dictionary));
        }
    }

    // If no "Catalog" found, abort.
    if catalog_object.is_none() {
        return Err("Catalog root not found.");
    }

    let catalog_object = catalog_object.unwrap();
    let pages_object = pages_object.unwrap();

    // Build a new "Pages" with updated fields
    if let Ok(dictionary) = pages_object.1.as_dict() {
        let mut dictionary = dictionary.clone();

        // Set new pages count
        dictionary.set("Count", documents_pages.len() as u32);

        // Set new "Kids" list (collected from documents pages) for "Pages"
        dictionary.set(
            "Kids",
            documents_pages
                .into_iter()
                .map(|(object_id, _)| Object::Reference(object_id))
                .collect::<Vec<_>>(),
        );

        document
            .objects
            .insert(pages_object.0, Object::Dictionary(dictionary));
    }

    // Build a new "Catalog" with updated fields
    if let Ok(dictionary) = catalog_object.1.as_dict() {
        let mut dictionary = dictionary.clone();
        dictionary.set("Pages", pages_object.0);
        dictionary.remove(b"Outlines"); // Outlines not supported in merged PDFs

        document
            .objects
            .insert(catalog_object.0, Object::Dictionary(dictionary));
    }

    document.trailer.set("Root", catalog_object.0);

    // Update the max internal ID as wasn't updated before due to direct objects insertion
    document.max_id = document.objects.len() as u32;

    // Reorder all new Document objects
    document.renumber_objects();

    // Set any Bookmarks to the First child if they are not set to a page
    document.adjust_zero_pages();

    // Set all bookmarks to the PDF Object tree then set the Outlines to the Bookmark content map.
    if let Some(n) = document.build_outline() {
        if let Ok(x) = document.get_object_mut(catalog_object.0) {
            if let Object::Dictionary(ref mut dict) = x {
                dict.set("Outlines", Object::Reference(n));
            }
        }
    }

    document.compress();

    return Ok(document);
}

fn main() {
    let args = Args::parse();
    let inpath = PathBuf::from(args.inpath);
    let outpath = PathBuf::from(args.outpath);
    println!("Path:\n    {:?}", inpath);
    let mut docs: Vec<(Document, String)> = load_documents_from_path(&inpath);
    if docs.len() == 0 {
        println!("No PDFs found");
        return;
    }
    docs.sort_by(|(_, a), (_, b)| a.cmp(b));
    println!("Order:");
    for (doc, name) in &docs {
        println!("    Title: {}, Pages: {}", name, doc.get_pages().len());
    }

    let merged_file_path = Path::new(&outpath);
    let merged_doc = merge(docs);
    match merged_doc {
        Ok(mut merged_doc) => {
            merged_doc.save(merged_file_path).unwrap();
            println!("Merged:");
            println!(
                "    Path: {}, Pages: {}",
                outpath.to_str().unwrap(),
                merged_doc.get_pages().len()
            );
        }
        Err(error_message) => println!("{}", error_message),
    }
}
