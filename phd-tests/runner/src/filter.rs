pub struct TestCaseFilter<'a> {
    pub must_include: &'a Vec<String>,
    pub must_exclude: &'a Vec<String>,
}

impl<'a> TestCaseFilter<'a> {
    pub fn check(&self, name: &str) -> bool {
        self.must_include.iter().all(|inc| name.contains(inc))
            && self.must_exclude.iter().all(|exc| !name.contains(exc))
    }
}
