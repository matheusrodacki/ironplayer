fn main() {
    // EmbedFiles: embute as fontes importadas no .slint (ui/fonts/*.ttf) no
    // binário — sem depender de caminhos absolutos da máquina de build.
    let config = slint_build::CompilerConfiguration::new()
        .embed_resources(slint_build::EmbedResourcesKind::EmbedFiles);
    slint_build::compile_with_config("ui/appwindow.slint", config)
        .expect("falha ao compilar appwindow.slint");
}
