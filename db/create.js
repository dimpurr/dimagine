const sqlite3 = require('sqlite3').verbose();
 
// open database in memory
let db = new sqlite3.Database('dimagine_alpha.db', (err) => {
  if (err) {
    return console.error(err.message);
  }
  console.log('Connected to the in-memory SQlite database.');
});
 
// close the database connection
db.close((err) => {
  if (err) {
    return console.error(err.message);
  }
  console.log('Close the database connection.');
});