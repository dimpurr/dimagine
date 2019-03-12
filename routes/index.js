var express = require('express');
var router = express.Router();

/* GET home page. */
router.get('/', function(req, res, next) {
  if (req.session.views) {
    req.session.views++
  } else {
    req.session.views = 1
  }
  res.render('index', {
    title: 'Dimagine',
    session_views: req.session.views
  });
});

/* GET login. */
router.get('/login', function(req, res, next) {
  res.send('respond with a login 1');
});

module.exports = router;
